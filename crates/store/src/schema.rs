//! Forward-only migrations. Append a new entry to `MIGRATIONS`; never edit an
//! applied one. The M0 schema covers workers, jobs, stages, logs, and events.
//! The remaining tables from the plan's data model arrive with the milestone
//! that first writes to them.

use anyhow::Result;
use rusqlite::Connection;

pub const MIGRATIONS: &[&str] = &[
    // 1: M0 core
    "CREATE TABLE worker (
         name          TEXT PRIMARY KEY,
         arch          TEXT,
         slots         INTEGER NOT NULL,
         mem_bytes     INTEGER,
         scratch_bytes INTEGER,
         first_seen    TEXT NOT NULL,
         last_seen     TEXT NOT NULL
     );
     CREATE TABLE job (
         id                 TEXT PRIMARY KEY,
         kind               TEXT NOT NULL,
         spec               TEXT NOT NULL,
         pkgbase            TEXT,
         stream             TEXT,
         state              TEXT NOT NULL,
         worker             TEXT,
         queued_at          TEXT NOT NULL,
         assigned_at        TEXT,
         started_at         TEXT,
         finished_at        TEXT,
         orphaned_at        TEXT,
         exit_code          INTEGER,
         error              TEXT,
         peak_rss_bytes     INTEGER,
         peak_scratch_bytes INTEGER,
         cpu_seconds        REAL,
         attempts           INTEGER NOT NULL DEFAULT 0
     );
     CREATE INDEX job_state_idx ON job(state, queued_at);
     CREATE INDEX job_worker_idx ON job(worker) WHERE worker IS NOT NULL;
     CREATE TABLE job_stage (
         job_id      TEXT NOT NULL REFERENCES job(id) ON DELETE CASCADE,
         stage       TEXT NOT NULL,
         started_at  TEXT NOT NULL,
         finished_at TEXT,
         PRIMARY KEY (job_id, stage)
     );
     CREATE TABLE job_log (
         job_id TEXT NOT NULL REFERENCES job(id) ON DELETE CASCADE,
         seq    INTEGER NOT NULL,
         ts     TEXT NOT NULL,
         fd     TEXT NOT NULL,
         line   TEXT NOT NULL,
         PRIMARY KEY (job_id, seq)
     );
     CREATE TABLE event (
         seq     INTEGER PRIMARY KEY AUTOINCREMENT,
         ts      TEXT NOT NULL,
         kind    TEXT NOT NULL,
         subject TEXT,
         payload TEXT
     );",
];

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
             version    INTEGER PRIMARY KEY,
             applied_at TEXT NOT NULL
         );",
    )?;
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get(0),
    )?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version <= current {
            continue;
        }
        tracing::info!(version, "applying schema migration");
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, repoman_common::clock::now()],
        )?;
        tx.commit()?;
    }
    Ok(())
}
