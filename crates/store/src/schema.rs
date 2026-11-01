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
    // 2: M1 packages, streams, runs, candidates, artifacts, publications, images
    "ALTER TABLE job ADD COLUMN candidate_id TEXT;
     ALTER TABLE job ADD COLUMN result TEXT;
     CREATE TABLE package (
         pkgbase          TEXT PRIMARY KEY,
         kind             TEXT NOT NULL DEFAULT 'release',   -- release | vcs
         provenance_class TEXT,
         policy           TEXT NOT NULL DEFAULT '{}',
         added_at         TEXT NOT NULL
     );
     CREATE TABLE package_stream (
         pkgbase  TEXT NOT NULL REFERENCES package(pkgbase) ON DELETE CASCADE,
         stream   TEXT NOT NULL,
         wanted   INTEGER NOT NULL DEFAULT 0,
         required INTEGER NOT NULL DEFAULT 0,
         PRIMARY KEY (pkgbase, stream)
     );
     CREATE TABLE baseline (
         pkgbase      TEXT PRIMARY KEY REFERENCES package(pkgbase) ON DELETE CASCADE,
         aur_commit   TEXT NOT NULL,
         srcinfo_hash TEXT,
         published_at TEXT NOT NULL,
         note         TEXT
     );
     CREATE TABLE aur_snapshot (
         pkgbase       TEXT PRIMARY KEY,
         fetched_at    TEXT NOT NULL,
         version       TEXT,
         last_modified INTEGER NOT NULL,
         maintainer    TEXT,
         submitter     TEXT,
         comaintainers TEXT NOT NULL DEFAULT '[]',
         out_of_date   INTEGER,
         votes         INTEGER,
         popularity    REAL,
         head_commit   TEXT
     );
     CREATE TABLE run (
         id          TEXT PRIMARY KEY,
         started_at  TEXT NOT NULL,
         finished_at TEXT,
         state       TEXT NOT NULL,
         step        TEXT,
         summary     TEXT NOT NULL DEFAULT '{}'
     );
     CREATE TABLE candidate (
         id                       TEXT PRIMARY KEY,
         run_id                   TEXT NOT NULL REFERENCES run(id),
         pkgbase                  TEXT NOT NULL,
         kind                     TEXT NOT NULL,
         aur_commit               TEXT,
         upstream_rev             TEXT,
         first_seen_last_modified INTEGER,
         first_seen_at            TEXT NOT NULL,
         class                    TEXT,
         floor_risk               TEXT,
         llm_risk                 TEXT,
         state                    TEXT NOT NULL,
         decided_at               TEXT,
         decided_by               TEXT,
         review                   TEXT NOT NULL DEFAULT '{}',
         note                     TEXT
     );
     CREATE INDEX candidate_pkgbase_idx ON candidate(pkgbase, state);
     CREATE INDEX candidate_run_idx ON candidate(run_id);
     CREATE TABLE signal (
         id          TEXT PRIMARY KEY,
         pkgbase     TEXT NOT NULL,
         aur_commit  TEXT,
         source      TEXT NOT NULL,
         severity    TEXT NOT NULL,
         payload     TEXT NOT NULL DEFAULT '{}',
         observed_at TEXT NOT NULL
     );
     CREATE INDEX signal_pkgbase_idx ON signal(pkgbase, aur_commit);
     CREATE TABLE corroboration (
         candidate_id TEXT NOT NULL REFERENCES candidate(id) ON DELETE CASCADE,
         check_name   TEXT NOT NULL,
         result       TEXT NOT NULL,
         detail       TEXT NOT NULL DEFAULT '{}',
         PRIMARY KEY (candidate_id, check_name)
     );
     CREATE TABLE base_image (
         sha256            TEXT PRIMARY KEY,
         stream            TEXT NOT NULL,
         filename          TEXT NOT NULL,
         size              INTEGER NOT NULL,
         created_at        TEXT NOT NULL,
         stream_fingerprint TEXT,
         packages          TEXT NOT NULL DEFAULT '[]',
         state             TEXT NOT NULL DEFAULT 'ready'   -- ready | retired
     );
     CREATE INDEX base_image_stream_idx ON base_image(stream, state, created_at);
     CREATE TABLE artifact (
         sha256       TEXT PRIMARY KEY,
         job_id       TEXT REFERENCES job(id),
         pkgbase      TEXT NOT NULL,
         pkgname      TEXT NOT NULL,
         pkgver       TEXT NOT NULL,
         arch         TEXT NOT NULL,
         stream       TEXT NOT NULL,
         filename     TEXT NOT NULL,
         size         INTEGER NOT NULL,
         sig_sha256   TEXT,
         aur_commit   TEXT,
         upstream_rev TEXT,
         depends      TEXT NOT NULL DEFAULT '[]',
         provides     TEXT NOT NULL DEFAULT '[]',
         paths        TEXT NOT NULL DEFAULT '[]',
         has_install  INTEGER NOT NULL DEFAULT 0,
         buildinfo    TEXT NOT NULL DEFAULT '{}',
         needed       TEXT,
         created_at   TEXT NOT NULL
     );
     CREATE INDEX artifact_pkg_idx ON artifact(pkgbase, pkgname, stream);
     CREATE TABLE publication (
         id           TEXT PRIMARY KEY,
         artifact_sha TEXT NOT NULL REFERENCES artifact(sha256),
         repo         TEXT NOT NULL,
         published_at TEXT NOT NULL,
         retired_at   TEXT
     );
     CREATE INDEX publication_repo_idx ON publication(repo, retired_at);
     CREATE INDEX publication_artifact_idx ON publication(artifact_sha);",
    // 3: packages may come from a git repository other than the AUR
    "ALTER TABLE package ADD COLUMN source_url TEXT;",
    // 4: the maintainer set at publication, for the gate's maintainer-change signal
    "ALTER TABLE baseline ADD COLUMN maintainers TEXT NOT NULL DEFAULT '[]';",
    // 5: setuid/setgid payload files, for the artifact diff
    "ALTER TABLE artifact ADD COLUMN setuid_paths TEXT NOT NULL DEFAULT '[]';",
    // 6: pull-through package cache bookkeeping
    "CREATE TABLE cache_file (
         filename           TEXT PRIMARY KEY,
         size               INTEGER NOT NULL,
         sha256             TEXT,
         stream             TEXT,
         repo               TEXT,
         fetched_at         TEXT NOT NULL,
         last_served_at     TEXT NOT NULL,
         serves             INTEGER NOT NULL DEFAULT 0,
         unreferenced_since TEXT
     );
     CREATE INDEX cache_file_lru_idx ON cache_file(unreferenced_since, last_served_at);",
    // 7: live progress of the active run, for the UI
    "ALTER TABLE run ADD COLUMN progress TEXT NOT NULL DEFAULT '{}';",
    // 8: AUR state. The base id and first-submitted date expose a delete-and-
    // resubmit; missing_since and orphaned_since time the two states that
    // matter. Signals gain an expiry (adoption windows) and an operator clear.
    "ALTER TABLE aur_snapshot ADD COLUMN package_base_id INTEGER;
     ALTER TABLE aur_snapshot ADD COLUMN first_submitted INTEGER;
     ALTER TABLE aur_snapshot ADD COLUMN missing_since TEXT;
     ALTER TABLE aur_snapshot ADD COLUMN orphaned_since TEXT;
     ALTER TABLE signal ADD COLUMN expires_at TEXT;
     ALTER TABLE signal ADD COLUMN cleared_at TEXT;
     ALTER TABLE signal ADD COLUMN cleared_by TEXT;",
    // 9: which run queued a job, so a run page can list its probes and
    // image builds, not only the builds its candidates own
    "ALTER TABLE job ADD COLUMN run_id TEXT;
     CREATE INDEX job_run_idx ON job(run_id) WHERE run_id IS NOT NULL;",
    // 10: operations have a kind (check | build | update) and a request;
    // the probe time lives on the snapshot now that probes are not jobs
    "ALTER TABLE run ADD COLUMN kind TEXT NOT NULL DEFAULT 'update';
     ALTER TABLE run ADD COLUMN request TEXT NOT NULL DEFAULT '{}';
     ALTER TABLE aur_snapshot ADD COLUMN probed_at TEXT;",
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
