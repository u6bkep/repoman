//! External signals about a package or one of its commits: security
//! advisories, AUR state changes, mailing-list reports. They raise risk and
//! block auto-approval; they never approve.

use anyhow::Result;
use repoman_common::clock;
use rusqlite::{OptionalExtension, params};
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
    /// After this instant the signal is history: shown, no longer blocking.
    pub expires_at: Option<String>,
    /// Set when the operator looked and decided it does not apply.
    pub cleared_at: Option<String>,
    pub cleared_by: Option<String>,
}

impl SignalRow {
    /// Blocks auto-approval: above `info`, not cleared, not expired.
    pub fn blocks(&self, now: &str) -> bool {
        self.severity != "info" && self.is_active(now)
    }

    pub fn is_active(&self, now: &str) -> bool {
        self.cleared_at.is_none() && self.expires_at.as_deref().is_none_or(|e| e > now)
    }

    /// The `kind` field of the payload, for sources that emit several kinds.
    pub fn kind(&self) -> Option<&str> {
        self.payload.get("kind").and_then(|v| v.as_str())
    }
}

const COLS: &str = "id, pkgbase, aur_commit, source, severity, payload, observed_at, expires_at, cleared_at, cleared_by";

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
        expires_at: r.get(7)?,
        cleared_at: r.get(8)?,
        cleared_by: r.get(9)?,
    })
}

impl Store {
    pub fn add_signal(&self, pkgbase: &str, aur_commit: Option<&str>, source: &str, severity: &str, payload: &serde_json::Value) -> Result<SignalRow> {
        self.add_signal_expiring(pkgbase, aur_commit, source, severity, payload, None)
    }

    pub fn add_signal_expiring(
        &self,
        pkgbase: &str,
        aur_commit: Option<&str>,
        source: &str,
        severity: &str,
        payload: &serde_json::Value,
        expires_at: Option<&str>,
    ) -> Result<SignalRow> {
        let id = repoman_common::ids::new_id();
        let now = clock::now();
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO signal ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)"),
                params![id, pkgbase, aur_commit, source, severity, payload.to_string(), now, expires_at],
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
            expires_at: expires_at.map(str::to_string),
            cleared_at: None,
            cleared_by: None,
        })
    }

    pub fn get_signal(&self, id: &str) -> Result<Option<SignalRow>> {
        self.with(|c| {
            Ok(c.query_row(&format!("SELECT {COLS} FROM signal WHERE id = ?1"), params![id], row)
                .optional()?)
        })
    }

    /// The operator looked and it does not apply. Returns false when the
    /// signal is unknown or already cleared.
    pub fn clear_signal(&self, id: &str, by: &str) -> Result<bool> {
        let now = clock::now();
        self.with(|c| {
            Ok(c.execute(
                "UPDATE signal SET cleared_at = ?2, cleared_by = ?3 WHERE id = ?1 AND cleared_at IS NULL",
                params![id, now, by],
            )? > 0)
        })
    }

    /// Active package-level signal from `source` whose payload `kind`
    /// matches, for deduplicating state signals across polls.
    pub fn active_signal(&self, pkgbase: &str, source: &str, kind: &str) -> Result<Option<SignalRow>> {
        let now = clock::now();
        Ok(self
            .signals_for(pkgbase, None)?
            .into_iter()
            .filter(|s| s.aur_commit.is_none() && s.source == source && s.kind() == Some(kind))
            .find(|s| s.is_active(&now)))
    }

    /// Every signal about a package, newest first, for the package page.
    pub fn signals_of(&self, pkgbase: &str) -> Result<Vec<SignalRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {COLS} FROM signal WHERE pkgbase = ?1 ORDER BY observed_at DESC"))?;
            Ok(st.query_map(params![pkgbase], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Signals that currently block something, across all packages.
    pub fn blocking_signals(&self) -> Result<Vec<SignalRow>> {
        let now = clock::now();
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {COLS} FROM signal WHERE severity != 'info' AND cleared_at IS NULL
                 AND (expires_at IS NULL OR expires_at > ?1) ORDER BY observed_at DESC"
            ))?;
            Ok(st.query_map(params![now], row)?.collect::<rusqlite::Result<Vec<_>>>()?)
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
        let now = clock::now();
        Ok(self.signals_for(pkgbase, commit)?.iter().any(|s| s.blocks(&now)))
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

    #[test]
    fn cleared_and_expired_signals_stop_blocking() {
        let s = Store::open_memory().unwrap();
        s.add_package("foo", "release", None).unwrap();
        let a = s.add_signal("foo", None, "aur", "medium", &serde_json::json!({"kind": "deleted"})).unwrap();
        assert!(s.has_negative_signal("foo", None).unwrap());
        assert!(s.active_signal("foo", "aur", "deleted").unwrap().is_some());
        assert!(s.clear_signal(&a.id, "ben").unwrap());
        assert!(!s.clear_signal(&a.id, "ben").unwrap(), "second clear is a no-op");
        assert!(!s.has_negative_signal("foo", None).unwrap());
        assert!(s.active_signal("foo", "aur", "deleted").unwrap().is_none());
        assert_eq!(s.get_signal(&a.id).unwrap().unwrap().cleared_by.as_deref(), Some("ben"));

        let past = "2000-01-01T00:00:00Z";
        s.add_signal_expiring("foo", None, "aur", "medium", &serde_json::json!({"kind": "adopted"}), Some(past)).unwrap();
        assert!(!s.has_negative_signal("foo", None).unwrap(), "expired");
        let future = "2999-01-01T00:00:00Z";
        s.add_signal_expiring("foo", None, "aur", "medium", &serde_json::json!({"kind": "adopted"}), Some(future)).unwrap();
        assert!(s.has_negative_signal("foo", None).unwrap());
        assert_eq!(s.signals_of("foo").unwrap().len(), 3);
        assert_eq!(s.blocking_signals().unwrap().len(), 1);
    }
}
