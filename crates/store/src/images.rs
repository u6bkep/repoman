//! Base images: content-addressed tarballs per stream.

use anyhow::Result;
use repoman_common::clock;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::Store;

#[derive(Debug, Clone, Serialize)]
pub struct ImageRow {
    pub sha256: String,
    pub stream: String,
    pub filename: String,
    pub size: u64,
    pub created_at: String,
    pub stream_fingerprint: Option<String>,
    pub packages: Vec<String>,
    pub state: String,
}

fn row_to_image(r: &rusqlite::Row) -> rusqlite::Result<ImageRow> {
    let pkgs: String = r.get(6)?;
    Ok(ImageRow {
        sha256: r.get(0)?,
        stream: r.get(1)?,
        filename: r.get(2)?,
        size: r.get::<_, i64>(3)? as u64,
        created_at: r.get(4)?,
        stream_fingerprint: r.get(5)?,
        packages: serde_json::from_str(&pkgs).unwrap_or_default(),
        state: r.get(7)?,
    })
}

const COLS: &str = "sha256, stream, filename, size, created_at, stream_fingerprint, packages, state";

impl Store {
    pub fn insert_image(
        &self,
        sha256: &str,
        stream: &str,
        filename: &str,
        size: u64,
        stream_fingerprint: Option<&str>,
        packages: &[String],
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO base_image (sha256, stream, filename, size, created_at, stream_fingerprint, packages, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'ready')
                 ON CONFLICT(sha256) DO UPDATE SET state = 'ready'",
                params![
                    sha256,
                    stream,
                    filename,
                    size as i64,
                    clock::now(),
                    stream_fingerprint,
                    serde_json::to_string(packages)?
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_image(&self, sha256: &str) -> Result<Option<ImageRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {COLS} FROM base_image WHERE sha256 = ?1"),
                params![sha256],
                row_to_image,
            )
            .optional()?)
        })
    }

    /// Newest ready image for a stream.
    pub fn latest_image(&self, stream: &str) -> Result<Option<ImageRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {COLS} FROM base_image WHERE stream = ?1 AND state = 'ready'
                     ORDER BY created_at DESC LIMIT 1"
                ),
                params![stream],
                row_to_image,
            )
            .optional()?)
        })
    }

    pub fn list_images(&self) -> Result<Vec<ImageRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {COLS} FROM base_image ORDER BY stream, created_at DESC"
            ))?;
            Ok(st
                .query_map([], row_to_image)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Retire every ready image of a stream except the newest `keep`.
    /// Returns the retired hashes.
    pub fn retire_old_images(&self, stream: &str, keep: usize) -> Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare(
                "UPDATE base_image SET state = 'retired'
                 WHERE stream = ?1 AND state = 'ready' AND sha256 NOT IN (
                     SELECT sha256 FROM base_image WHERE stream = ?1 AND state = 'ready'
                     ORDER BY created_at DESC LIMIT ?2)
                 RETURNING sha256",
            )?;
            Ok(st
                .query_map(params![stream, keep as i64], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn delete_image(&self, sha256: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM base_image WHERE sha256 = ?1", params![sha256])?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_and_retire() {
        let s = Store::open_memory().unwrap();
        s.insert_image("a", "stable", "a.tar.zst", 10, Some("fp1"), &["base 1".into()]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.insert_image("b", "stable", "b.tar.zst", 11, Some("fp2"), &[]).unwrap();
        s.insert_image("c", "unstable", "c.tar.zst", 12, None, &[]).unwrap();
        assert_eq!(s.latest_image("stable").unwrap().unwrap().sha256, "b");
        assert_eq!(s.retire_old_images("stable", 1).unwrap(), vec!["a".to_string()]);
        assert_eq!(s.get_image("a").unwrap().unwrap().state, "retired");
        assert_eq!(s.latest_image("stable").unwrap().unwrap().sha256, "b");
        assert_eq!(s.latest_image("unstable").unwrap().unwrap().packages.len(), 0);
        assert_eq!(s.get_image("a").unwrap().unwrap().packages, ["base 1"]);
    }
}
