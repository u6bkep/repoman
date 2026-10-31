//! Packages, stream membership, baselines, and AUR snapshots.

use anyhow::Result;
use repoman_common::clock;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRow {
    pub pkgbase: String,
    pub kind: String,
    pub provenance_class: Option<String>,
    pub policy: serde_json::Value,
    pub added_at: String,
    /// Git URL of the PKGBUILD repository when not the AUR.
    pub source_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipRow {
    pub pkgbase: String,
    pub stream: String,
    pub wanted: bool,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineRow {
    pub pkgbase: String,
    pub aur_commit: String,
    pub srcinfo_hash: Option<String>,
    pub published_at: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRow {
    pub pkgbase: String,
    pub fetched_at: String,
    pub version: Option<String>,
    pub last_modified: i64,
    pub maintainer: Option<String>,
    pub submitter: Option<String>,
    pub comaintainers: Vec<String>,
    pub out_of_date: Option<i64>,
    pub votes: Option<i64>,
    pub popularity: Option<f64>,
    pub head_commit: Option<String>,
}

impl Store {
    /// Insert a package if missing. Existing rows are left untouched.
    pub fn add_package(&self, pkgbase: &str, kind: &str, class: Option<&str>) -> Result<bool> {
        self.add_package_from(pkgbase, kind, class, None)
    }

    /// Insert a package with an explicit git source. `None` means the AUR.
    pub fn add_package_from(&self, pkgbase: &str, kind: &str, class: Option<&str>, source_url: Option<&str>) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO package (pkgbase, kind, provenance_class, added_at, source_url)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![pkgbase, kind, class, clock::now(), source_url],
            )?;
            Ok(n == 1)
        })
    }

    pub fn set_package_source(&self, pkgbase: &str, source_url: Option<&str>) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE package SET source_url = ?2 WHERE pkgbase = ?1",
                params![pkgbase, source_url],
            )? == 1)
        })
    }

    pub fn get_package(&self, pkgbase: &str) -> Result<Option<PackageRow>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT pkgbase, kind, provenance_class, policy, added_at, source_url FROM package WHERE pkgbase = ?1",
                params![pkgbase],
                |r| {
                    let policy: String = r.get(3)?;
                    Ok(PackageRow {
                        pkgbase: r.get(0)?,
                        kind: r.get(1)?,
                        provenance_class: r.get(2)?,
                        policy: serde_json::from_str(&policy).unwrap_or(serde_json::json!({})),
                        added_at: r.get(4)?,
                        source_url: r.get(5)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub fn list_packages(&self) -> Result<Vec<PackageRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT pkgbase, kind, provenance_class, policy, added_at, source_url FROM package ORDER BY pkgbase",
            )?;
            Ok(st
                .query_map([], |r| {
                    let policy: String = r.get(3)?;
                    Ok(PackageRow {
                        pkgbase: r.get(0)?,
                        kind: r.get(1)?,
                        provenance_class: r.get(2)?,
                        policy: serde_json::from_str(&policy).unwrap_or(serde_json::json!({})),
                        added_at: r.get(4)?,
                        source_url: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn set_package_class(&self, pkgbase: &str, class: &str) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE package SET provenance_class = ?2 WHERE pkgbase = ?1",
                params![pkgbase, class],
            )? == 1)
        })
    }

    pub fn set_package_kind(&self, pkgbase: &str, kind: &str) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE package SET kind = ?2 WHERE pkgbase = ?1",
                params![pkgbase, kind],
            )? == 1)
        })
    }

    pub fn set_package_policy(&self, pkgbase: &str, policy: &serde_json::Value) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE package SET policy = ?2 WHERE pkgbase = ?1",
                params![pkgbase, policy.to_string()],
            )? == 1)
        })
    }

    pub fn remove_package(&self, pkgbase: &str) -> Result<bool> {
        self.with(|c| Ok(c.execute("DELETE FROM package WHERE pkgbase = ?1", params![pkgbase])? == 1))
    }

    // ---- membership -------------------------------------------------------

    pub fn set_membership(&self, pkgbase: &str, stream: &str, wanted: bool, required: bool) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO package_stream (pkgbase, stream, wanted, required) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(pkgbase, stream) DO UPDATE SET wanted = excluded.wanted, required = excluded.required",
                params![pkgbase, stream, wanted, required],
            )?;
            Ok(())
        })
    }

    /// Mark a package as required on a stream without touching `wanted`.
    pub fn mark_required(&self, pkgbase: &str, stream: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO package_stream (pkgbase, stream, wanted, required) VALUES (?1, ?2, 0, 1)
                 ON CONFLICT(pkgbase, stream) DO UPDATE SET required = 1",
                params![pkgbase, stream],
            )?;
            Ok(())
        })
    }

    pub fn clear_required(&self, stream: &str) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE package_stream SET required = 0 WHERE stream = ?1", params![stream])?;
            Ok(())
        })
    }

    pub fn remove_membership(&self, pkgbase: &str, stream: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM package_stream WHERE pkgbase = ?1 AND stream = ?2",
                params![pkgbase, stream],
            )?;
            Ok(())
        })
    }

    pub fn memberships(&self, pkgbase: &str) -> Result<Vec<MembershipRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT pkgbase, stream, wanted, required FROM package_stream WHERE pkgbase = ?1 ORDER BY stream",
            )?;
            Ok(st
                .query_map(params![pkgbase], row_to_membership)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Every (pkgbase, stream) that is wanted or required.
    pub fn all_memberships(&self) -> Result<Vec<MembershipRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT pkgbase, stream, wanted, required FROM package_stream
                 WHERE wanted = 1 OR required = 1 ORDER BY stream, pkgbase",
            )?;
            Ok(st
                .query_map([], row_to_membership)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn members_of_stream(&self, stream: &str) -> Result<Vec<MembershipRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT pkgbase, stream, wanted, required FROM package_stream
                 WHERE stream = ?1 AND (wanted = 1 OR required = 1) ORDER BY pkgbase",
            )?;
            Ok(st
                .query_map(params![stream], row_to_membership)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- baseline -----------------------------------------------------------

    pub fn get_baseline(&self, pkgbase: &str) -> Result<Option<BaselineRow>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT pkgbase, aur_commit, srcinfo_hash, published_at, note FROM baseline WHERE pkgbase = ?1",
                params![pkgbase],
                |r| {
                    Ok(BaselineRow {
                        pkgbase: r.get(0)?,
                        aur_commit: r.get(1)?,
                        srcinfo_hash: r.get(2)?,
                        published_at: r.get(3)?,
                        note: r.get(4)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub fn set_baseline(&self, pkgbase: &str, aur_commit: &str, srcinfo_hash: Option<&str>, note: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO baseline (pkgbase, aur_commit, srcinfo_hash, published_at, note)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(pkgbase) DO UPDATE SET aur_commit = excluded.aur_commit,
                    srcinfo_hash = excluded.srcinfo_hash, published_at = excluded.published_at,
                    note = excluded.note",
                params![pkgbase, aur_commit, srcinfo_hash, clock::now(), note],
            )?;
            Ok(())
        })
    }

    // ---- AUR snapshot -----------------------------------------------------

    pub fn get_snapshot(&self, pkgbase: &str) -> Result<Option<SnapshotRow>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT pkgbase, fetched_at, version, last_modified, maintainer, submitter, comaintainers,
                        out_of_date, votes, popularity, head_commit
                 FROM aur_snapshot WHERE pkgbase = ?1",
                params![pkgbase],
                row_to_snapshot,
            )
            .optional()?)
        })
    }

    pub fn upsert_snapshot(&self, s: &SnapshotRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO aur_snapshot (pkgbase, fetched_at, version, last_modified, maintainer, submitter,
                                           comaintainers, out_of_date, votes, popularity, head_commit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(pkgbase) DO UPDATE SET fetched_at = excluded.fetched_at, version = excluded.version,
                    last_modified = excluded.last_modified, maintainer = excluded.maintainer,
                    submitter = excluded.submitter, comaintainers = excluded.comaintainers,
                    out_of_date = excluded.out_of_date, votes = excluded.votes,
                    popularity = excluded.popularity,
                    head_commit = COALESCE(excluded.head_commit, aur_snapshot.head_commit)",
                params![
                    s.pkgbase,
                    s.fetched_at,
                    s.version,
                    s.last_modified,
                    s.maintainer,
                    s.submitter,
                    serde_json::to_string(&s.comaintainers)?,
                    s.out_of_date,
                    s.votes,
                    s.popularity,
                    s.head_commit,
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_snapshot_head(&self, pkgbase: &str, head: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE aur_snapshot SET head_commit = ?2 WHERE pkgbase = ?1",
                params![pkgbase, head],
            )?;
            Ok(())
        })
    }
}

fn row_to_membership(r: &rusqlite::Row) -> rusqlite::Result<MembershipRow> {
    Ok(MembershipRow {
        pkgbase: r.get(0)?,
        stream: r.get(1)?,
        wanted: r.get(2)?,
        required: r.get(3)?,
    })
}

fn row_to_snapshot(r: &rusqlite::Row) -> rusqlite::Result<SnapshotRow> {
    let co: String = r.get(6)?;
    Ok(SnapshotRow {
        pkgbase: r.get(0)?,
        fetched_at: r.get(1)?,
        version: r.get(2)?,
        last_modified: r.get(3)?,
        maintainer: r.get(4)?,
        submitter: r.get(5)?,
        comaintainers: serde_json::from_str(&co).unwrap_or_default(),
        out_of_date: r.get(7)?,
        votes: r.get(8)?,
        popularity: r.get(9)?,
        head_commit: r.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_memberships_baselines() {
        let s = Store::open_memory().unwrap();
        assert!(s.add_package("yay", "release", Some("upstream-source")).unwrap());
        assert!(!s.add_package("yay", "release", None).unwrap(), "idempotent");
        s.set_membership("yay", "stable", true, false).unwrap();
        s.mark_required("yay", "unstable").unwrap();
        let m = s.memberships("yay").unwrap();
        assert_eq!(m.len(), 2);
        assert!(m[0].wanted && !m[0].required);
        assert!(!m[1].wanted && m[1].required);
        s.clear_required("unstable").unwrap();
        assert_eq!(s.members_of_stream("unstable").unwrap().len(), 0);
        assert_eq!(s.members_of_stream("stable").unwrap().len(), 1);

        assert!(s.get_baseline("yay").unwrap().is_none());
        s.set_baseline("yay", "abc", Some("h1"), Some("onboarded")).unwrap();
        let b = s.get_baseline("yay").unwrap().unwrap();
        assert_eq!(b.aur_commit, "abc");
        s.set_baseline("yay", "def", None, None).unwrap();
        assert_eq!(s.get_baseline("yay").unwrap().unwrap().aur_commit, "def");

        s.set_package_policy("yay", &serde_json::json!({"pgp": "skip"})).unwrap();
        assert_eq!(s.get_package("yay").unwrap().unwrap().policy["pgp"], "skip");
        assert!(s.remove_package("yay").unwrap());
        assert!(s.memberships("yay").unwrap().is_empty(), "cascade");
    }

    #[test]
    fn snapshot_upsert_keeps_head_when_null() {
        let s = Store::open_memory().unwrap();
        let mut snap = SnapshotRow {
            pkgbase: "yay".into(),
            fetched_at: clock::now(),
            version: Some("1-1".into()),
            last_modified: 100,
            maintainer: Some("a".into()),
            submitter: None,
            comaintainers: vec!["b".into()],
            out_of_date: None,
            votes: Some(3),
            popularity: Some(1.5),
            head_commit: Some("h1".into()),
        };
        s.upsert_snapshot(&snap).unwrap();
        snap.head_commit = None;
        snap.last_modified = 200;
        s.upsert_snapshot(&snap).unwrap();
        let got = s.get_snapshot("yay").unwrap().unwrap();
        assert_eq!(got.last_modified, 200);
        assert_eq!(got.head_commit.as_deref(), Some("h1"));
        assert_eq!(got.comaintainers, ["b"]);
    }
}
