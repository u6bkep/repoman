//! External signals about a package or one of its commits: security
//! advisories, AUR state changes, mailing-list reports. They raise risk and
//! block auto-approval; they never approve.

use anyhow::Result;
use repoman_common::clock;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::Store;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignalRow {
    pub id: String,
    pub pkgbase: String,
    pub aur_commit: Option<String>,
    pub source: String,
    /// `info` is shown but does not block; anything else blocks
    /// auto-approval of the commit (or the package, when commit is null).
    pub severity: String,
    pub payload: serde_json::Value,
    pub observed_at: String,
}

const COLS: &str = "id, pkgbase, aur_commit, source, severity, payload, observed_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<SignalRow> {
    let payload: String = r.get(5)?;
    Ok(SignalRow {
        id: r.get(0)?,
        pkgbase: r.get(1)?,
        aur_commit: r.get(2)?,
        source: r.get(3)?,
        severity: r.get(4)?,
        payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
        observed_at: r.get(6)?,
    })
}

impl Store {
    pub fn add_signal(&self, pkgbase: &str, aur_commit: Option<&str>, source: &str, severity: &str, payload: &serde_json::Value) -> Result<SignalRow> {
        let id = repoman_common::ids::new_id();
        let now = clock::now();
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO signal ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
                params![id, pkgbase, aur_commit, source, severity, payload.to_string(), now],
            )?;
            Ok(())
        })?;
        Ok(SignalRow {
            id,
            pkgbase: pkgbase.to_string(),
            aur_commit: aur_commit.map(str::to_string),
            source: source.to_string(),
            severity: severity.to_string(),
            payload: payload.clone(),
            observed_at: now,
        })
    }

    /// Signals about a package: those pinned to `commit` plus those about the
    /// package as a whole.
    pub fn signals_for(&self, pkgbase: &str, commit: Option<&str>) -> Result<Vec<SignalRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {COLS} FROM signal WHERE pkgbase = ?1 AND (aur_commit IS NULL OR aur_commit = ?2) ORDER BY observed_at"
            ))?;
            Ok(st.query_map(params![pkgbase, commit], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn has_negative_signal(&self, pkgbase: &str, commit: Option<&str>) -> Result<bool> {
        Ok(self.signals_for(pkgbase, commit)?.iter().any(|s| s.severity != "info"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_scope_to_commit_or_package() {
        let s = Store::open_memory().unwrap();
        s.add_package("foo", "release", None).unwrap();
        s.add_signal("foo", Some("abc"), "aur-security", "high", &serde_json::json!({"id": 1})).unwrap();
        s.add_signal("foo", None, "aur", "info", &serde_json::json!({"orphaned": true})).unwrap();
        assert_eq!(s.signals_for("foo", Some("abc")).unwrap().len(), 2);
        assert_eq!(s.signals_for("foo", Some("def")).unwrap().len(), 1);
        assert!(s.has_negative_signal("foo", Some("abc")).unwrap());
        assert!(!s.has_negative_signal("foo", Some("def")).unwrap());
    }
}
