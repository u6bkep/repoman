//! Bookkeeping for the pull-through package cache: one row per cached
//! upstream file with its provenance, size, last-served time, and when it
//! stopped being referenced by any stream database.

use anyhow::Result;
use repoman_common::clock;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::Store;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheFileRow {
    pub filename: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub stream: Option<String>,
    pub repo: Option<String>,
    pub fetched_at: String,
    pub last_served_at: String,
    pub serves: u64,
    /// Set when the file left every stream's database; cleared if it returns.
    pub unreferenced_since: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub files: u64,
    pub bytes: u64,
    pub referenced_bytes: u64,
    pub unreferenced_files: u64,
    pub unreferenced_bytes: u64,
}

const COLS: &str = "filename, size, sha256, stream, repo, fetched_at, last_served_at, serves, unreferenced_since";

fn row(r: &rusqlite::Row) -> rusqlite::Result<CacheFileRow> {
    Ok(CacheFileRow {
        filename: r.get(0)?,
        size: r.get::<_, i64>(1)? as u64,
        sha256: r.get(2)?,
        stream: r.get(3)?,
        repo: r.get(4)?,
        fetched_at: r.get(5)?,
        last_served_at: r.get(6)?,
        serves: r.get::<_, i64>(7)? as u64,
        unreferenced_since: r.get(8)?,
    })
}

impl Store {
    pub fn cache_insert(&self, filename: &str, size: u64, sha256: Option<&str>, stream: Option<&str>, repo: Option<&str>) -> Result<()> {
        let now = clock::now();
        self.with(|c| {
            c.execute(
                &format!(
                    "INSERT INTO cache_file ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 0, NULL)
                     ON CONFLICT(filename) DO UPDATE SET size = excluded.size, sha256 = excluded.sha256,
                        stream = excluded.stream, repo = excluded.repo, fetched_at = excluded.fetched_at,
                        unreferenced_since = NULL"
                ),
                params![filename, size as i64, sha256, stream, repo, now],
            )?;
            Ok(())
        })
    }

    pub fn cache_touch(&self, filename: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE cache_file SET last_served_at = ?2, serves = serves + 1 WHERE filename = ?1",
                params![filename, clock::now()],
            )?;
            Ok(())
        })
    }

    pub fn cache_get(&self, filename: &str) -> Result<Option<CacheFileRow>> {
        self.with(|c| {
            Ok(c.query_row(&format!("SELECT {COLS} FROM cache_file WHERE filename = ?1"), params![filename], row)
                .optional()?)
        })
    }

    pub fn cache_delete(&self, filename: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM cache_file WHERE filename = ?1", params![filename])?;
            Ok(())
        })
    }

    pub fn cache_list(&self) -> Result<Vec<CacheFileRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {COLS} FROM cache_file ORDER BY last_served_at DESC"))?;
            Ok(st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Reconcile with the current referenced set: files not in it start
    /// their grace period (once), files back in it are cleared.
    pub fn cache_mark_referenced(&self, referenced: &std::collections::HashSet<String>) -> Result<(usize, usize)> {
        let now = clock::now();
        self.with(|c| {
            let names: Vec<String> = {
                let mut st = c.prepare("SELECT filename FROM cache_file")?;
                st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let (mut newly, mut back) = (0, 0);
            for n in names {
                if referenced.contains(&n) {
                    back += c.execute(
                        "UPDATE cache_file SET unreferenced_since = NULL WHERE filename = ?1 AND unreferenced_since IS NOT NULL",
                        params![n],
                    )?;
                } else {
                    newly += c.execute(
                        "UPDATE cache_file SET unreferenced_since = ?2 WHERE filename = ?1 AND unreferenced_since IS NULL",
                        params![n, now],
                    )?;
                }
            }
            Ok((newly, back))
        })
    }

    /// Files whose grace period ended before `cutoff` (a clock string).
    pub fn cache_expired(&self, cutoff: &str) -> Result<Vec<CacheFileRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {COLS} FROM cache_file WHERE unreferenced_since IS NOT NULL AND unreferenced_since < ?1 ORDER BY last_served_at"
            ))?;
            Ok(st.query_map(params![cutoff], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Evictable (unreferenced) files, least recently served first, for the
    /// size cap.
    pub fn cache_evictable_lru(&self) -> Result<Vec<CacheFileRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {COLS} FROM cache_file WHERE unreferenced_since IS NOT NULL ORDER BY last_served_at ASC"
            ))?;
            Ok(st.query_map([], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn cache_stats(&self) -> Result<CacheStats> {
        self.with(|c| {
            let (files, bytes): (i64, i64) = c.query_row("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM cache_file", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let (ufiles, ubytes): (i64, i64) = c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM cache_file WHERE unreferenced_since IS NOT NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(CacheStats {
                files: files as u64,
                bytes: bytes as u64,
                referenced_bytes: (bytes - ubytes) as u64,
                unreferenced_files: ufiles as u64,
                unreferenced_bytes: ubytes as u64,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn reference_marking_and_eviction_order() {
        let s = Store::open_memory().unwrap();
        s.cache_insert("a-1-x86_64.pkg.tar.zst", 10, Some("aa"), Some("s1"), Some("core")).unwrap();
        s.cache_insert("b-1-x86_64.pkg.tar.zst", 20, None, Some("s1"), Some("core")).unwrap();
        s.cache_touch("b-1-x86_64.pkg.tar.zst").unwrap();
        let referenced: HashSet<String> = ["a-1-x86_64.pkg.tar.zst".to_string()].into();
        assert_eq!(s.cache_mark_referenced(&referenced).unwrap(), (1, 0));
        assert_eq!(s.cache_mark_referenced(&referenced).unwrap(), (0, 0), "grace start is recorded once");
        let st = s.cache_stats().unwrap();
        assert_eq!((st.files, st.bytes, st.referenced_bytes, st.unreferenced_bytes), (2, 30, 10, 20));
        assert!(s.cache_expired("2000-01-01T00:00:00.000Z").unwrap().is_empty());
        assert_eq!(s.cache_expired("2999-01-01T00:00:00.000Z").unwrap().len(), 1);
        let all: HashSet<String> = ["a-1-x86_64.pkg.tar.zst".to_string(), "b-1-x86_64.pkg.tar.zst".to_string()].into();
        assert_eq!(s.cache_mark_referenced(&all).unwrap(), (0, 1), "a file back in the databases leaves grace");
        assert!(s.cache_evictable_lru().unwrap().is_empty());
        s.cache_delete("a-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!(s.cache_stats().unwrap().files, 1);
        assert_eq!(s.cache_get("b-1-x86_64.pkg.tar.zst").unwrap().unwrap().serves, 1);
    }
}
