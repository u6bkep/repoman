//! SQLite persistence for the coordinator. One connection behind a mutex:
//! the coordinator is the single writer by design, and every operation here
//! is a handful of indexed statements, so holding the lock across an async
//! boundary is never necessary.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use repoman_common::clock;
use repoman_common::proto::{Fd, JobId, JobKind, JobState, LogLine, Outcome, Peaks};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;

mod schema;
pub mod artifacts;
pub mod cache;
pub mod images;
pub mod packages;
pub mod runs;
pub mod signals;

pub use artifacts::{ArtifactRow, PublicationRow};
pub use images::ImageRow;
pub use packages::{BaselineRow, MembershipRow, PackageRow, SnapshotRow};
pub use runs::{CandidateRow, NewCandidate, RunRow};

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerRow {
    pub name: String,
    pub arch: Option<String>,
    pub slots: u32,
    pub mem_bytes: Option<u64>,
    pub scratch_bytes: Option<u64>,
    pub first_seen: String,
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRow {
    pub id: JobId,
    pub job: JobKind,
    pub pkgbase: Option<String>,
    pub stream: Option<String>,
    pub state: JobState,
    pub worker: Option<String>,
    pub queued_at: String,
    pub assigned_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub orphaned_at: Option<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub peaks: Peaks,
    pub attempts: u32,
    pub candidate_id: Option<String>,
    pub result: Option<repoman_common::proto::JobResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventRow {
    pub seq: i64,
    pub ts: String,
    pub kind: String,
    pub subject: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageRow {
    pub stage: String,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// Result of reconciling a reconnecting worker's `in_flight` list.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Jobs the worker still holds; now `running` on this worker.
    pub resumed: Vec<JobId>,
    /// Jobs the coordinator thought the worker held but the worker does not
    /// know about; back to `queued`.
    pub requeued: Vec<JobId>,
    /// Jobs the worker claims that are held by another worker. Left alone.
    pub conflicts: Vec<JobId>,
}

pub(crate) const JOB_COLS: &str = "id, spec, pkgbase, stream, state, worker, queued_at, assigned_at, \
                        started_at, finished_at, orphaned_at, exit_code, error, \
                        peak_rss_bytes, peak_scratch_bytes, cpu_seconds, attempts, candidate_id, result";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        schema::migrate(&conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        f(&conn)
    }

    // ---- events -----------------------------------------------------------

    /// Newest events first.
    pub fn recent_events(&self, limit: u32) -> Result<Vec<EventRow>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT seq, ts, kind, subject, payload FROM event ORDER BY seq DESC LIMIT ?1")?;
            Ok(st
                .query_map(params![limit], |r| {
                    let payload: Option<String> = r.get(4)?;
                    Ok(EventRow {
                        seq: r.get(0)?,
                        ts: r.get(1)?,
                        kind: r.get(2)?,
                        subject: r.get(3)?,
                        payload: payload.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or(serde_json::Value::Null),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn event(&self, kind: &str, subject: Option<&str>, payload: serde_json::Value) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO event (ts, kind, subject, payload) VALUES (?1, ?2, ?3, ?4)",
                params![clock::now(), kind, subject, payload.to_string()],
            )?;
            Ok(())
        })
    }

    // ---- workers ----------------------------------------------------------

    pub fn upsert_worker(
        &self,
        name: &str,
        arch: &str,
        slots: u32,
        mem_bytes: Option<u64>,
        scratch_bytes: Option<u64>,
    ) -> Result<()> {
        self.with(|c| {
            let now = clock::now();
            c.execute(
                "INSERT INTO worker (name, arch, slots, mem_bytes, scratch_bytes, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(name) DO UPDATE SET
                   arch = excluded.arch, slots = excluded.slots,
                   mem_bytes = excluded.mem_bytes, scratch_bytes = excluded.scratch_bytes,
                   last_seen = excluded.last_seen",
                params![name, arch, slots, mem_bytes.map(|v| v as i64), scratch_bytes.map(|v| v as i64), now],
            )?;
            Ok(())
        })
    }

    pub fn touch_worker(&self, name: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE worker SET last_seen = ?2 WHERE name = ?1",
                params![name, clock::now()],
            )?;
            Ok(())
        })
    }

    pub fn list_workers(&self) -> Result<Vec<WorkerRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT name, arch, slots, mem_bytes, scratch_bytes, first_seen, last_seen
                 FROM worker ORDER BY name",
            )?;
            let rows = st
                .query_map([], |r| {
                    Ok(WorkerRow {
                        name: r.get(0)?,
                        arch: r.get(1)?,
                        slots: r.get(2)?,
                        mem_bytes: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                        scratch_bytes: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                        first_seen: r.get(5)?,
                        last_seen: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ---- jobs -------------------------------------------------------------

    pub fn create_job(
        &self,
        job: &JobKind,
        pkgbase: Option<&str>,
        stream: Option<&str>,
    ) -> Result<JobRow> {
        self.create_job_for(job, pkgbase, stream, None)
    }

    pub fn create_job_for(
        &self,
        job: &JobKind,
        pkgbase: Option<&str>,
        stream: Option<&str>,
        candidate_id: Option<&str>,
    ) -> Result<JobRow> {
        let id = repoman_common::new_id();
        self.with(|c| {
            c.execute(
                "INSERT INTO job (id, kind, spec, pkgbase, stream, state, queued_at, candidate_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7)",
                params![
                    id,
                    job.name(),
                    serde_json::to_string(job)?,
                    pkgbase,
                    stream,
                    clock::now(),
                    candidate_id
                ],
            )?;
            Ok(())
        })?;
        self.get_job(&id)?.context("job vanished after insert")
    }

    pub fn get_job(&self, id: &str) -> Result<Option<JobRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {JOB_COLS} FROM job WHERE id = ?1"),
                params![id],
                row_to_job,
            )
            .optional()?)
        })
    }

    pub fn list_jobs(&self, limit: u32) -> Result<Vec<JobRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {JOB_COLS} FROM job ORDER BY queued_at DESC, id DESC LIMIT ?1"
            ))?;
            Ok(st
                .query_map(params![limit], row_to_job)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// (peak RSS, peak scratch) of the newest successful builds of a package
    /// base, newest first, for footprint prediction.
    pub fn recent_build_peaks(&self, pkgbase: &str, limit: u32) -> Result<Vec<(Option<u64>, Option<u64>)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT peak_rss_bytes, peak_scratch_bytes FROM job
                 WHERE kind = 'build' AND pkgbase = ?1 AND state = 'done' AND finished_at IS NOT NULL
                 ORDER BY finished_at DESC LIMIT ?2",
            )?;
            Ok(st
                .query_map(params![pkgbase, limit], |r| {
                    Ok((r.get::<_, Option<i64>>(0)?.map(|v| v as u64), r.get::<_, Option<i64>>(1)?.map(|v| v as u64)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Oldest first, so dispatch is FIFO.
    pub fn queued_jobs(&self) -> Result<Vec<JobRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {JOB_COLS} FROM job WHERE state = 'queued' ORDER BY queued_at, id"
            ))?;
            Ok(st
                .query_map([], row_to_job)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn set_assigned(&self, id: &str, worker: &str) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE job SET state = 'assigned', worker = ?2, assigned_at = ?3,
                        attempts = attempts + 1, orphaned_at = NULL
                 WHERE id = ?1 AND state = 'queued'",
                params![id, worker, clock::now()],
            )?;
            Ok(n == 1)
        })
    }

    /// Called on `Accepted`. Also tolerated from `orphaned`, for a worker
    /// whose acceptance was in flight when it dropped and reconnected.
    pub fn set_running(&self, id: &str, worker: &str) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE job SET state = 'running', worker = ?2,
                        started_at = COALESCE(started_at, ?3), orphaned_at = NULL
                 WHERE id = ?1 AND state IN ('assigned', 'orphaned')",
                params![id, worker, clock::now()],
            )?;
            Ok(n == 1)
        })
    }

    /// Return an assigned job to the queue, e.g. when the send to the worker
    /// fails before the worker acknowledged it.
    pub fn requeue(&self, id: &str) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE job SET state = 'queued', worker = NULL, assigned_at = NULL, orphaned_at = NULL
                 WHERE id = ?1 AND state IN ('assigned', 'orphaned')",
                params![id],
            )?;
            Ok(n == 1)
        })
    }

    pub fn finish_job(&self, id: &str, worker: &str, outcome: &Outcome) -> Result<bool> {
        let state = if outcome.exit_code == 0 && outcome.error.is_none() {
            JobState::Done
        } else {
            JobState::Failed
        };
        self.with(|c| {
            let n = c.execute(
                "UPDATE job SET state = ?2, finished_at = ?3, exit_code = ?4, error = ?5,
                        peak_rss_bytes = ?6, peak_scratch_bytes = ?7, cpu_seconds = ?8,
                        orphaned_at = NULL, started_at = COALESCE(started_at, ?3), result = ?10
                 WHERE id = ?1 AND worker = ?9 AND state IN ('assigned', 'running', 'orphaned')",
                params![
                    id,
                    state.as_str(),
                    clock::now(),
                    outcome.exit_code,
                    outcome.error,
                    outcome.peaks.peak_rss_bytes.map(|v| v as i64),
                    outcome.peaks.peak_scratch_bytes.map(|v| v as i64),
                    outcome.peaks.cpu_seconds,
                    worker,
                    outcome.result.as_ref().map(|r| serde_json::to_string(r).unwrap_or_default()),
                ],
            )?;
            Ok(n == 1)
        })
    }

    /// The worker's connection dropped: everything it held becomes orphaned.
    pub fn orphan_worker_jobs(&self, worker: &str) -> Result<Vec<JobId>> {
        self.with(|c| {
            let now = clock::now();
            let mut st = c.prepare(
                "UPDATE job SET state = 'orphaned', orphaned_at = ?2
                 WHERE worker = ?1 AND state IN ('assigned', 'running')
                 RETURNING id",
            )?;
            Ok(st
                .query_map(params![worker, now], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// A worker (re)connected and reports the jobs it is still executing.
    pub fn reconcile_worker(&self, worker: &str, in_flight: &[JobId]) -> Result<Reconciled> {
        let claimed: HashSet<&str> = in_flight.iter().map(String::as_str).collect();
        self.with(|c| {
            let mut out = Reconciled::default();
            let now = clock::now();

            // Jobs we attributed to this worker.
            let held: Vec<(String, String)> = {
                let mut st = c.prepare(
                    "SELECT id, state FROM job
                     WHERE worker = ?1 AND state IN ('assigned', 'running', 'orphaned')",
                )?;
                st.query_map(params![worker], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for (id, _state) in held {
                if claimed.contains(id.as_str()) {
                    c.execute(
                        "UPDATE job SET state = 'running', orphaned_at = NULL,
                                started_at = COALESCE(started_at, ?2) WHERE id = ?1",
                        params![id, now],
                    )?;
                    out.resumed.push(id);
                } else {
                    c.execute(
                        "UPDATE job SET state = 'queued', worker = NULL, assigned_at = NULL,
                                orphaned_at = NULL WHERE id = ?1",
                        params![id],
                    )?;
                    out.requeued.push(id);
                }
            }

            // Jobs the worker claims that we did not attribute to it.
            for id in in_flight {
                if out.resumed.contains(id) {
                    continue;
                }
                let row: Option<(String, Option<String>)> = c
                    .query_row(
                        "SELECT state, worker FROM job WHERE id = ?1",
                        params![id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                match row {
                    // Requeued after grace, not yet taken by anyone: give it back.
                    Some((state, _)) if state == "queued" => {
                        c.execute(
                            "UPDATE job SET state = 'running', worker = ?2,
                                    started_at = COALESCE(started_at, ?3), attempts = attempts + 1
                             WHERE id = ?1",
                            params![id, worker, now],
                        )?;
                        out.resumed.push(id.clone());
                    }
                    Some((state, w)) if JobState::parse(&state).is_some_and(JobState::is_held)
                        && w.as_deref() != Some(worker) =>
                    {
                        out.conflicts.push(id.clone());
                    }
                    // Terminal or unknown: nothing to do.
                    _ => {}
                }
            }
            Ok(out)
        })
    }

    /// Cancel every non-terminal job of the given kinds. Used when a run
    /// resumes after the coordinator restarted: the new plan creates fresh
    /// jobs, and results from the old ones would be ignored anyway.
    pub fn cancel_open_jobs(&self, kinds: &[&str]) -> Result<Vec<JobId>> {
        self.with(|c| {
            let placeholders = kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut st = c.prepare(&format!(
                "UPDATE job SET state = 'cancelled', finished_at = ?, error = 'cancelled: run resumed'
                 WHERE kind IN ({placeholders}) AND state IN ('queued', 'assigned', 'running', 'orphaned')
                 RETURNING id"
            ))?;
            let now = clock::now();
            let mut p: Vec<&dyn rusqlite::ToSql> = vec![&now];
            for k in kinds {
                p.push(k);
            }
            Ok(st
                .query_map(p.as_slice(), |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Orphans older than `grace_secs` go back to the queue.
    pub fn requeue_stale_orphans(&self, grace_secs: u64) -> Result<Vec<JobId>> {
        self.with(|c| {
            let cutoff = clock::ago(grace_secs);
            let mut st = c.prepare(
                "UPDATE job SET state = 'queued', worker = NULL, assigned_at = NULL, orphaned_at = NULL
                 WHERE state = 'orphaned' AND orphaned_at <= ?1
                 RETURNING id",
            )?;
            Ok(st
                .query_map(params![cutoff], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    // ---- stages and logs --------------------------------------------------

    pub fn stage_started(&self, job: &str, stage: &str, ts: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO job_stage (job_id, stage, started_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(job_id, stage) DO UPDATE SET started_at = excluded.started_at,
                                                          finished_at = NULL",
                params![job, stage, ts],
            )?;
            Ok(())
        })
    }

    pub fn stage_finished(&self, job: &str, stage: &str, ts: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE job_stage SET finished_at = ?3 WHERE job_id = ?1 AND stage = ?2",
                params![job, stage, ts],
            )?;
            Ok(())
        })
    }

    pub fn job_stages(&self, job: &str) -> Result<Vec<StageRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT stage, started_at, finished_at FROM job_stage
                 WHERE job_id = ?1 ORDER BY started_at",
            )?;
            Ok(st
                .query_map(params![job], |r| {
                    Ok(StageRow {
                        stage: r.get(0)?,
                        started_at: r.get(1)?,
                        finished_at: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn append_logs(&self, job: &str, lines: &[LogLine]) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            let next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM job_log WHERE job_id = ?1",
                params![job],
                |r| r.get(0),
            )?;
            let mut st = tx.prepare_cached(
                "INSERT INTO job_log (job_id, seq, ts, fd, line) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (i, l) in lines.iter().enumerate() {
                st.execute(params![job, next + i as i64, l.ts, l.fd.as_str(), l.line])?;
            }
            drop(st);
            tx.commit()?;
            Ok(())
        })
    }

    pub fn job_log(&self, job: &str) -> Result<Vec<LogLine>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT ts, fd, line FROM job_log WHERE job_id = ?1 ORDER BY seq",
            )?;
            Ok(st
                .query_map(params![job], |r| {
                    let fd: String = r.get(1)?;
                    Ok(LogLine {
                        ts: r.get(0)?,
                        fd: Fd::parse(&fd).unwrap_or(Fd::System),
                        line: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

pub(crate) fn row_to_job(r: &Row) -> rusqlite::Result<JobRow> {
    let spec: String = r.get(1)?;
    let job: JobKind = serde_json::from_str(&spec).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let state: String = r.get(4)?;
    let state = JobState::parse(&state).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            format!("unknown job state {state:?}").into(),
        )
    })?;
    Ok(JobRow {
        id: r.get(0)?,
        job,
        pkgbase: r.get(2)?,
        stream: r.get(3)?,
        state,
        worker: r.get(5)?,
        queued_at: r.get(6)?,
        assigned_at: r.get(7)?,
        started_at: r.get(8)?,
        finished_at: r.get(9)?,
        orphaned_at: r.get(10)?,
        exit_code: r.get(11)?,
        error: r.get(12)?,
        peaks: Peaks {
            peak_rss_bytes: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
            peak_scratch_bytes: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
            cpu_seconds: r.get(15)?,
        },
        attempts: r.get(16)?,
        candidate_id: r.get(17)?,
        result: r
            .get::<_, Option<String>>(18)?
            .and_then(|s| serde_json::from_str(&s).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop() -> JobKind {
        JobKind::Noop {
            duration_ms: 1,
            fail: false,
        }
    }

    #[test]
    fn job_lifecycle_happy_path() {
        let s = Store::open_memory().unwrap();
        let j = s.create_job(&noop(), Some("foo"), None).unwrap();
        assert_eq!(j.state, JobState::Queued);
        assert_eq!(s.queued_jobs().unwrap().len(), 1);

        assert!(s.set_assigned(&j.id, "w1").unwrap());
        assert!(!s.set_assigned(&j.id, "w2").unwrap(), "already assigned");
        assert!(s.set_running(&j.id, "w1").unwrap());
        s.stage_started(&j.id, "build", "2026-01-01T00:00:00.000Z").unwrap();
        s.stage_finished(&j.id, "build", "2026-01-01T00:00:01.000Z").unwrap();
        s.append_logs(
            &j.id,
            &[LogLine {
                ts: "2026-01-01T00:00:00.500Z".into(),
                fd: Fd::Stdout,
                line: "hi".into(),
            }],
        )
        .unwrap();
        s.append_logs(
            &j.id,
            &[LogLine {
                ts: "2026-01-01T00:00:00.600Z".into(),
                fd: Fd::Stderr,
                line: "warn".into(),
            }],
        )
        .unwrap();

        assert!(!s.finish_job(&j.id, "w2", &Outcome::success()).unwrap(), "wrong worker");
        assert!(s.finish_job(&j.id, "w1", &Outcome::success()).unwrap());
        assert!(!s.finish_job(&j.id, "w1", &Outcome::success()).unwrap(), "duplicate finish");

        let j = s.get_job(&j.id).unwrap().unwrap();
        assert_eq!(j.state, JobState::Done);
        assert_eq!(j.attempts, 1);
        assert_eq!(j.worker.as_deref(), Some("w1"));
        let log = s.job_log(&j.id).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].fd, Fd::Stderr);
        assert_eq!(s.job_stages(&j.id).unwrap()[0].finished_at.as_deref(), Some("2026-01-01T00:00:01.000Z"));
    }

    #[test]
    fn failure_outcome_marks_failed() {
        let s = Store::open_memory().unwrap();
        let j = s.create_job(&noop(), None, None).unwrap();
        s.set_assigned(&j.id, "w").unwrap();
        s.set_running(&j.id, "w").unwrap();
        s.finish_job(&j.id, "w", &Outcome::failure(1, "boom")).unwrap();
        let j = s.get_job(&j.id).unwrap().unwrap();
        assert_eq!(j.state, JobState::Failed);
        assert_eq!(j.error.as_deref(), Some("boom"));
    }

    #[test]
    fn orphan_then_resume() {
        let s = Store::open_memory().unwrap();
        let a = s.create_job(&noop(), None, None).unwrap();
        let b = s.create_job(&noop(), None, None).unwrap();
        for j in [&a, &b] {
            s.set_assigned(&j.id, "w").unwrap();
            s.set_running(&j.id, "w").unwrap();
        }
        let orphaned = s.orphan_worker_jobs("w").unwrap();
        assert_eq!(orphaned.len(), 2);

        // Worker returns knowing only about `a`.
        let rec = s.reconcile_worker("w", std::slice::from_ref(&a.id)).unwrap();
        assert_eq!(rec.resumed, vec![a.id.clone()]);
        assert_eq!(rec.requeued, vec![b.id.clone()]);
        assert_eq!(s.get_job(&a.id).unwrap().unwrap().state, JobState::Running);
        assert_eq!(s.get_job(&b.id).unwrap().unwrap().state, JobState::Queued);
        assert!(s.finish_job(&a.id, "w", &Outcome::success()).unwrap());
    }

    #[test]
    fn stale_orphans_requeue_and_late_worker_reclaims() {
        let s = Store::open_memory().unwrap();
        let j = s.create_job(&noop(), None, None).unwrap();
        s.set_assigned(&j.id, "w").unwrap();
        s.set_running(&j.id, "w").unwrap();
        s.orphan_worker_jobs("w").unwrap();
        assert!(s.requeue_stale_orphans(3600).unwrap().is_empty(), "inside grace");
        assert_eq!(s.requeue_stale_orphans(0).unwrap(), vec![j.id.clone()]);
        assert_eq!(s.get_job(&j.id).unwrap().unwrap().state, JobState::Queued);

        // The original worker comes back still running it before anyone else took it.
        let rec = s.reconcile_worker("w", std::slice::from_ref(&j.id)).unwrap();
        assert_eq!(rec.resumed, vec![j.id.clone()]);
        let row = s.get_job(&j.id).unwrap().unwrap();
        assert_eq!(row.state, JobState::Running);
        assert_eq!(row.worker.as_deref(), Some("w"));
    }

    #[test]
    fn reconcile_reports_conflict_for_job_held_elsewhere() {
        let s = Store::open_memory().unwrap();
        let j = s.create_job(&noop(), None, None).unwrap();
        s.set_assigned(&j.id, "other").unwrap();
        let rec = s.reconcile_worker("w", std::slice::from_ref(&j.id)).unwrap();
        assert_eq!(rec.conflicts, vec![j.id.clone()]);
        assert!(rec.resumed.is_empty());
    }

    #[test]
    fn migrations_are_idempotent() {
        let s = Store::open_memory().unwrap();
        s.with(|c| {
            schema::migrate(c)?;
            let v: i64 = c.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?;
            assert_eq!(v as usize, schema::MIGRATIONS.len());
            Ok(())
        })
        .unwrap();
    }
}
