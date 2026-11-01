//! Built artifacts in the content store and their publications in repos.

use anyhow::Result;
use repoman_common::clock;
use repoman_common::proto::ArtifactInfo;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::Store;

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactRow {
    pub sha256: String,
    pub job_id: Option<String>,
    pub pkgbase: String,
    pub pkgname: String,
    pub pkgver: String,
    pub arch: String,
    pub stream: String,
    pub filename: String,
    pub size: u64,
    pub sig_sha256: Option<String>,
    pub aur_commit: Option<String>,
    pub upstream_rev: Option<String>,
    pub depends: Vec<String>,
    pub provides: Vec<String>,
    pub paths: Vec<String>,
    pub has_install: bool,
    pub setuid_paths: Vec<String>,
    pub buildinfo: serde_json::Value,
    pub needed: Option<serde_json::Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublicationRow {
    pub id: String,
    pub artifact_sha: String,
    pub repo: String,
    pub published_at: String,
    pub retired_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveVersion {
    pub pkgbase: String,
    pub repo: String,
    pub pkgname: String,
    pub pkgver: String,
    pub published_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactPublication {
    pub artifact: ArtifactRow,
    pub repo: String,
    pub published_at: String,
    pub retired_at: Option<String>,
}

const COLS: &str = "sha256, job_id, pkgbase, pkgname, pkgver, arch, stream, filename, size, sig_sha256, \
                    aur_commit, upstream_rev, depends, provides, paths, has_install, buildinfo, needed, created_at, setuid_paths";

fn row_to_artifact(r: &rusqlite::Row) -> rusqlite::Result<ArtifactRow> {
    let json = |i: usize| -> rusqlite::Result<serde_json::Value> {
        let s: Option<String> = r.get(i)?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::Value::Null))
    };
    let strs = |i: usize| -> rusqlite::Result<Vec<String>> {
        let s: String = r.get(i)?;
        Ok(serde_json::from_str(&s).unwrap_or_default())
    };
    let needed = json(17)?;
    Ok(ArtifactRow {
        sha256: r.get(0)?,
        job_id: r.get(1)?,
        pkgbase: r.get(2)?,
        pkgname: r.get(3)?,
        pkgver: r.get(4)?,
        arch: r.get(5)?,
        stream: r.get(6)?,
        filename: r.get(7)?,
        size: r.get::<_, i64>(8)? as u64,
        sig_sha256: r.get(9)?,
        aur_commit: r.get(10)?,
        upstream_rev: r.get(11)?,
        depends: strs(12)?,
        provides: strs(13)?,
        paths: strs(14)?,
        has_install: r.get(15)?,
        buildinfo: json(16)?,
        needed: if needed.is_null() { None } else { Some(needed) },
        created_at: r.get(18)?,
        setuid_paths: strs(19)?,
    })
}

fn row_to_publication(r: &rusqlite::Row) -> rusqlite::Result<PublicationRow> {
    Ok(PublicationRow {
        id: r.get(0)?,
        artifact_sha: r.get(1)?,
        repo: r.get(2)?,
        published_at: r.get(3)?,
        retired_at: r.get(4)?,
    })
}

impl Store {
    pub fn insert_artifact(
        &self,
        info: &ArtifactInfo,
        job_id: Option<&str>,
        stream: &str,
        aur_commit: Option<&str>,
        upstream_rev: Option<&str>,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO artifact (sha256, job_id, pkgbase, pkgname, pkgver, arch, stream, filename, size,
                                       aur_commit, upstream_rev, depends, provides, paths, has_install, buildinfo, created_at, setuid_paths)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
                 ON CONFLICT(sha256) DO UPDATE SET job_id = excluded.job_id",
                params![
                    info.sha256,
                    job_id,
                    info.pkgbase,
                    info.pkgname,
                    info.pkgver,
                    info.arch,
                    stream,
                    info.filename,
                    info.size as i64,
                    aur_commit,
                    upstream_rev,
                    serde_json::to_string(&info.depends)?,
                    serde_json::to_string(&info.provides)?,
                    serde_json::to_string(&info.paths)?,
                    info.has_install_script,
                    info.buildinfo.to_string(),
                    clock::now(),
                    serde_json::to_string(&info.setuid_paths)?,
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_artifact_signature(&self, sha256: &str, sig_sha256: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE artifact SET sig_sha256 = ?2 WHERE sha256 = ?1",
                params![sha256, sig_sha256],
            )?;
            Ok(())
        })
    }

    pub fn set_artifact_needed(&self, sha256: &str, needed: &serde_json::Value) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE artifact SET needed = ?2 WHERE sha256 = ?1",
                params![sha256, needed.to_string()],
            )?;
            Ok(())
        })
    }

    pub fn get_artifact(&self, sha256: &str) -> Result<Option<ArtifactRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {COLS} FROM artifact WHERE sha256 = ?1"),
                params![sha256],
                row_to_artifact,
            )
            .optional()?)
        })
    }

    pub fn artifacts_for_job(&self, job_id: &str) -> Result<Vec<ArtifactRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {COLS} FROM artifact WHERE job_id = ?1 ORDER BY pkgname"
            ))?;
            Ok(st
                .query_map(params![job_id], row_to_artifact)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- publications -----------------------------------------------------

    /// Publish an artifact to a repo, retiring any live publication of the
    /// same pkgname in that repo. Returns the retired artifact hashes.
    pub fn publish(&self, artifact_sha: &str, repo: &str) -> Result<Vec<String>> {
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let now = clock::now();
            let pkgname: String = tx.query_row(
                "SELECT pkgname FROM artifact WHERE sha256 = ?1",
                params![artifact_sha],
                |r| r.get(0),
            )?;
            let retired: Vec<String> = {
                let mut st = tx.prepare(
                    "UPDATE publication SET retired_at = ?3
                     WHERE repo = ?1 AND retired_at IS NULL AND artifact_sha != ?2
                       AND artifact_sha IN (SELECT sha256 FROM artifact WHERE pkgname = ?4)
                     RETURNING artifact_sha",
                )?;
                st.query_map(params![repo, artifact_sha, now, pkgname], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let already: Option<String> = tx
                .query_row(
                    "SELECT id FROM publication WHERE repo = ?1 AND artifact_sha = ?2 AND retired_at IS NULL",
                    params![repo, artifact_sha],
                    |r| r.get(0),
                )
                .optional()?;
            if already.is_none() {
                tx.execute(
                    "INSERT INTO publication (id, artifact_sha, repo, published_at) VALUES (?1, ?2, ?3, ?4)",
                    params![repoman_common::new_id(), artifact_sha, repo, now],
                )?;
            }
            tx.commit()?;
            Ok(retired)
        })
    }

    pub fn retire_publication(&self, artifact_sha: &str, repo: &str) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE publication SET retired_at = ?3 WHERE repo = ?1 AND artifact_sha = ?2 AND retired_at IS NULL",
                params![repo, artifact_sha, clock::now()],
            )? >= 1)
        })
    }

    /// Live artifacts in a repo, for building its database.
    pub fn live_artifacts(&self, repo: &str) -> Result<Vec<ArtifactRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {} FROM artifact a JOIN publication p ON p.artifact_sha = a.sha256
                 WHERE p.repo = ?1 AND p.retired_at IS NULL ORDER BY a.pkgname",
                COLS.split(", ").map(|col| format!("a.{col}")).collect::<Vec<_>>().join(", ")
            ))?;
            Ok(st
                .query_map(params![repo], row_to_artifact)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// What each repo serves right now, for every pkgbase: (pkgbase, repo,
    /// pkgver, published_at), one row per live pkgname.
    pub fn live_versions_all(&self) -> Result<Vec<LiveVersion>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT a.pkgbase, p.repo, a.pkgname, a.pkgver, p.published_at
                 FROM artifact a JOIN publication p ON p.artifact_sha = a.sha256
                 WHERE p.retired_at IS NULL ORDER BY a.pkgbase, p.repo, a.pkgname",
            )?;
            Ok(st
                .query_map([], |r| {
                    Ok(LiveVersion { pkgbase: r.get(0)?, repo: r.get(1)?, pkgname: r.get(2)?, pkgver: r.get(3)?, published_at: r.get(4)? })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Live artifact of a pkgname in a repo, if any.
    pub fn live_artifact(&self, repo: &str, pkgname: &str) -> Result<Option<ArtifactRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {} FROM artifact a JOIN publication p ON p.artifact_sha = a.sha256
                     WHERE p.repo = ?1 AND p.retired_at IS NULL AND a.pkgname = ?2",
                    COLS.split(", ").map(|col| format!("a.{col}")).collect::<Vec<_>>().join(", ")
                ),
                params![repo, pkgname],
                row_to_artifact,
            )
            .optional()?)
        })
    }

    /// Resolve a served filename in a repo: live first, then retained.
    pub fn artifact_by_filename(&self, repo: &str, filename: &str) -> Result<Option<ArtifactRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {} FROM artifact a JOIN publication p ON p.artifact_sha = a.sha256
                     WHERE p.repo = ?1 AND a.filename = ?2
                     ORDER BY p.retired_at IS NULL DESC, p.published_at DESC LIMIT 1",
                    COLS.split(", ").map(|col| format!("a.{col}")).collect::<Vec<_>>().join(", ")
                ),
                params![repo, filename],
                row_to_artifact,
            )
            .optional()?)
        })
    }

    /// Every publication of a package base's artifacts, newest first, for
    /// the package history view.
    pub fn artifact_history(&self, pkgbase: &str) -> Result<Vec<ArtifactPublication>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {}, p.repo, p.published_at, p.retired_at FROM artifact a
                 JOIN publication p ON p.artifact_sha = a.sha256
                 WHERE a.pkgbase = ?1 ORDER BY p.published_at DESC, a.pkgname",
                COLS.split(", ").map(|col| format!("a.{col}")).collect::<Vec<_>>().join(", ")
            ))?;
            let n = COLS.split(", ").count();
            Ok(st
                .query_map(params![pkgbase], |r| {
                    Ok(ArtifactPublication {
                        artifact: row_to_artifact(r)?,
                        repo: r.get(n)?,
                        published_at: r.get(n + 1)?,
                        retired_at: r.get(n + 2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn publications_of(&self, artifact_sha: &str) -> Result<Vec<PublicationRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id, artifact_sha, repo, published_at, retired_at FROM publication
                 WHERE artifact_sha = ?1 ORDER BY published_at",
            )?;
            Ok(st
                .query_map(params![artifact_sha], row_to_publication)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Delete retired publications beyond the newest `keep` per pkgname per
    /// repo. Returns artifact hashes whose last publication went away and
    /// that are therefore eligible for store garbage collection.
    pub fn prune_retired(&self, repo: &str, keep: usize) -> Result<Vec<String>> {
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let doomed: Vec<(String, String)> = {
                let mut st = tx.prepare(
                    "SELECT p.id, p.artifact_sha FROM publication p JOIN artifact a ON a.sha256 = p.artifact_sha
                     WHERE p.repo = ?1 AND p.retired_at IS NOT NULL
                       AND p.id NOT IN (
                           SELECT p2.id FROM publication p2 JOIN artifact a2 ON a2.sha256 = p2.artifact_sha
                           WHERE p2.repo = ?1 AND p2.retired_at IS NOT NULL AND a2.pkgname = a.pkgname
                           ORDER BY p2.retired_at DESC LIMIT ?2)",
                )?;
                st.query_map(params![repo, keep as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let mut orphaned = Vec::new();
            for (id, sha) in doomed {
                tx.execute("DELETE FROM publication WHERE id = ?1", params![id])?;
                let remaining: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM publication WHERE artifact_sha = ?1",
                    params![sha],
                    |r| r.get(0),
                )?;
                if remaining == 0 {
                    orphaned.push(sha);
                }
            }
            tx.commit()?;
            Ok(orphaned)
        })
    }

    pub fn delete_artifact(&self, sha256: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM artifact WHERE sha256 = ?1", params![sha256])?;
            Ok(())
        })
    }

    /// Previously published artifact of the same pkgname in a repo, for the
    /// artifact diff: the most recently retired one.
    pub fn previous_artifact(&self, repo: &str, pkgname: &str) -> Result<Option<ArtifactRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {} FROM artifact a JOIN publication p ON p.artifact_sha = a.sha256
                     WHERE p.repo = ?1 AND a.pkgname = ?2 AND p.retired_at IS NOT NULL
                     ORDER BY p.retired_at DESC LIMIT 1",
                    COLS.split(", ").map(|col| format!("a.{col}")).collect::<Vec<_>>().join(", ")
                ),
                params![repo, pkgname],
                row_to_artifact,
            )
            .optional()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(sha: &str, pkgname: &str, ver: &str) -> ArtifactInfo {
        ArtifactInfo {
            sha256: sha.into(),
            filename: format!("{pkgname}-{ver}-x86_64.pkg.tar.zst"),
            size: 1,
            pkgname: pkgname.into(),
            pkgbase: pkgname.into(),
            pkgver: ver.into(),
            arch: "x86_64".into(),
            depends: vec![],
            provides: vec![],
            paths: vec!["usr/bin/x".into()],
            has_install_script: false,
            setuid_paths: vec![],
            needed: vec![],
            buildinfo: serde_json::json!({}),
        }
    }

    #[test]
    fn publish_retires_and_prunes() {
        let s = Store::open_memory().unwrap();
        for (sha, ver) in [("s1", "1-1"), ("s2", "2-1"), ("s3", "3-1"), ("s4", "4-1")] {
            s.insert_artifact(&info(sha, "foo", ver), None, "stable", Some("c"), None).unwrap();
        }
        assert!(s.publish("s1", "stable").unwrap().is_empty());
        assert_eq!(s.publish("s2", "stable").unwrap(), vec!["s1".to_string()]);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(s.publish("s3", "stable").unwrap(), vec!["s2".to_string()]);
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(s.publish("s4", "stable").unwrap(), vec!["s3".to_string()]);
        assert!(s.publish("s4", "stable").unwrap().is_empty(), "idempotent republish");

        let live = s.live_artifacts("stable").unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].sha256, "s4");
        assert_eq!(s.live_artifact("stable", "foo").unwrap().unwrap().pkgver, "4-1");
        assert_eq!(s.previous_artifact("stable", "foo").unwrap().unwrap().sha256, "s3");
        assert_eq!(
            s.artifact_by_filename("stable", "foo-2-1-x86_64.pkg.tar.zst").unwrap().unwrap().sha256,
            "s2",
            "retained versions still resolve"
        );

        // Keep 1 retired: s1 and s2 go, s3 stays.
        let mut gc = s.prune_retired("stable", 1).unwrap();
        gc.sort();
        assert_eq!(gc, ["s1", "s2"]);
        assert!(s.artifact_by_filename("stable", "foo-2-1-x86_64.pkg.tar.zst").unwrap().is_none());
        assert_eq!(s.previous_artifact("stable", "foo").unwrap().unwrap().sha256, "s3");
    }

    #[test]
    fn any_artifact_live_in_two_repos_is_not_gc_candidate() {
        let s = Store::open_memory().unwrap();
        s.insert_artifact(&info("a1", "doc", "1-1"), None, "stable", None, None).unwrap();
        s.insert_artifact(&info("a2", "doc", "2-1"), None, "stable", None, None).unwrap();
        s.publish("a1", "stable").unwrap();
        s.publish("a1", "unstable").unwrap();
        s.publish("a2", "stable").unwrap();
        // a1 retired in stable, still live in unstable
        assert!(s.prune_retired("stable", 0).unwrap().is_empty());
        assert_eq!(s.live_artifact("unstable", "doc").unwrap().unwrap().sha256, "a1");
    }
}
