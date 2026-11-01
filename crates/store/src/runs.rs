//! Update runs and their candidates.

use anyhow::Result;
use repoman_common::clock;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRow {
    pub id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub state: String,
    pub step: Option<String>,
    pub summary: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRow {
    pub id: String,
    pub run_id: String,
    pub pkgbase: String,
    pub kind: String,
    pub aur_commit: Option<String>,
    pub upstream_rev: Option<String>,
    pub first_seen_last_modified: Option<i64>,
    pub first_seen_at: String,
    pub class: Option<String>,
    pub floor_risk: Option<String>,
    pub llm_risk: Option<String>,
    pub state: String,
    pub decided_at: Option<String>,
    pub decided_by: Option<String>,
    pub review: serde_json::Value,
    pub note: Option<String>,
}

/// Candidate states that are still in play.
pub const OPEN_STATES: &[&str] = &["pending", "approved", "deferred", "building", "built", "publish_review", "failed"];

#[derive(Debug, Clone, Default)]
pub struct NewCandidate<'a> {
    pub pkgbase: &'a str,
    pub kind: &'a str,
    pub aur_commit: Option<&'a str>,
    pub upstream_rev: Option<&'a str>,
    pub first_seen_last_modified: Option<i64>,
    pub class: Option<&'a str>,
    pub floor_risk: Option<&'a str>,
    pub state: &'a str,
    pub review: serde_json::Value,
}

const CAND_COLS: &str = "id, run_id, pkgbase, kind, aur_commit, upstream_rev, first_seen_last_modified, first_seen_at, \
                         class, floor_risk, llm_risk, state, decided_at, decided_by, review, note";

fn row_to_candidate(r: &rusqlite::Row) -> rusqlite::Result<CandidateRow> {
    let review: String = r.get(14)?;
    Ok(CandidateRow {
        id: r.get(0)?,
        run_id: r.get(1)?,
        pkgbase: r.get(2)?,
        kind: r.get(3)?,
        aur_commit: r.get(4)?,
        upstream_rev: r.get(5)?,
        first_seen_last_modified: r.get(6)?,
        first_seen_at: r.get(7)?,
        class: r.get(8)?,
        floor_risk: r.get(9)?,
        llm_risk: r.get(10)?,
        state: r.get(11)?,
        decided_at: r.get(12)?,
        decided_by: r.get(13)?,
        review: serde_json::from_str(&review).unwrap_or(serde_json::json!({})),
        note: r.get(15)?,
    })
}

fn row_to_run(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    let summary: String = r.get(5)?;
    Ok(RunRow {
        id: r.get(0)?,
        started_at: r.get(1)?,
        finished_at: r.get(2)?,
        state: r.get(3)?,
        step: r.get(4)?,
        summary: serde_json::from_str(&summary).unwrap_or(serde_json::json!({})),
    })
}

impl Store {
    // ---- runs -------------------------------------------------------------

    pub fn create_run(&self) -> Result<RunRow> {
        let id = repoman_common::new_id();
        self.with(|c| {
            c.execute(
                "INSERT INTO run (id, started_at, state) VALUES (?1, ?2, 'running')",
                params![id, clock::now()],
            )?;
            Ok(())
        })?;
        Ok(self.get_run(&id)?.expect("run just inserted"))
    }

    pub fn get_run(&self, id: &str) -> Result<Option<RunRow>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id, started_at, finished_at, state, step, summary FROM run WHERE id = ?1",
                params![id],
                row_to_run,
            )
            .optional()?)
        })
    }

    /// The run that is not finished, if any. Only one may be active.
    pub fn active_run(&self) -> Result<Option<RunRow>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id, started_at, finished_at, state, step, summary FROM run
                 WHERE finished_at IS NULL ORDER BY started_at DESC LIMIT 1",
                [],
                row_to_run,
            )
            .optional()?)
        })
    }

    pub fn list_runs(&self, limit: u32) -> Result<Vec<RunRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id, started_at, finished_at, state, step, summary FROM run
                 ORDER BY started_at DESC LIMIT ?1",
            )?;
            Ok(st
                .query_map(params![limit], row_to_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn set_run_step(&self, id: &str, state: &str, step: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE run SET state = ?2, step = ?3 WHERE id = ?1",
                params![id, state, step],
            )?;
            Ok(())
        })
    }

    pub fn finish_run(&self, id: &str, state: &str, summary: &serde_json::Value) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE run SET state = ?2, finished_at = ?3, summary = ?4, step = NULL WHERE id = ?1",
                params![id, state, clock::now(), summary.to_string()],
            )?;
            Ok(())
        })
    }

    // ---- candidates -------------------------------------------------------

    pub fn create_candidate(&self, run_id: &str, n: &NewCandidate) -> Result<CandidateRow> {
        let id = repoman_common::new_id();
        self.with(|c| {
            c.execute(
                "INSERT INTO candidate (id, run_id, pkgbase, kind, aur_commit, upstream_rev, first_seen_last_modified,
                                        first_seen_at, class, floor_risk, state, review)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    id,
                    run_id,
                    n.pkgbase,
                    n.kind,
                    n.aur_commit,
                    n.upstream_rev,
                    n.first_seen_last_modified,
                    clock::now(),
                    n.class,
                    n.floor_risk,
                    n.state,
                    n.review.to_string()
                ],
            )?;
            Ok(())
        })?;
        Ok(self.get_candidate(&id)?.expect("candidate just inserted"))
    }

    pub fn get_candidate(&self, id: &str) -> Result<Option<CandidateRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {CAND_COLS} FROM candidate WHERE id = ?1"),
                params![id],
                row_to_candidate,
            )
            .optional()?)
        })
    }

    /// Open candidates for a package, newest first.
    pub fn open_candidates(&self, pkgbase: &str) -> Result<Vec<CandidateRow>> {
        self.with(|c| {
            let placeholders = OPEN_STATES.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut st = c.prepare(&format!(
                "SELECT {CAND_COLS} FROM candidate WHERE pkgbase = ?1 AND state IN ({placeholders})
                 ORDER BY first_seen_at DESC"
            ))?;
            let mut p: Vec<&dyn rusqlite::ToSql> = vec![&pkgbase];
            for s in OPEN_STATES {
                p.push(s);
            }
            Ok(st
                .query_map(p.as_slice(), row_to_candidate)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Latest rejected candidate for a package, to honour "rejected until the
    /// head moves".
    pub fn rejected_candidate_for(&self, pkgbase: &str, aur_commit: &str) -> Result<Option<CandidateRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {CAND_COLS} FROM candidate
                     WHERE pkgbase = ?1 AND aur_commit = ?2 AND state = 'rejected'
                     ORDER BY decided_at DESC LIMIT 1"
                ),
                params![pkgbase, aur_commit],
                row_to_candidate,
            )
            .optional()?)
        })
    }

    pub fn candidates_in_states(&self, states: &[&str]) -> Result<Vec<CandidateRow>> {
        self.with(|c| {
            let placeholders = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut st = c.prepare(&format!(
                "SELECT {CAND_COLS} FROM candidate WHERE state IN ({placeholders}) ORDER BY first_seen_at"
            ))?;
            let p: Vec<&dyn rusqlite::ToSql> = states.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            Ok(st
                .query_map(p.as_slice(), row_to_candidate)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn candidates_for_run(&self, run_id: &str) -> Result<Vec<CandidateRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {CAND_COLS} FROM candidate WHERE run_id = ?1 ORDER BY first_seen_at"
            ))?;
            Ok(st
                .query_map(params![run_id], row_to_candidate)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn set_candidate_state(&self, id: &str, state: &str) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE candidate SET state = ?2 WHERE id = ?1",
                params![id, state],
            )? == 1)
        })
    }

    pub fn decide_candidate(&self, id: &str, state: &str, by: &str, note: Option<&str>) -> Result<bool> {
        self.with(|c| {
            Ok(c.execute(
                "UPDATE candidate SET state = ?2, decided_at = ?3, decided_by = ?4,
                        note = COALESCE(?5, note) WHERE id = ?1",
                params![id, state, clock::now(), by, note],
            )? == 1)
        })
    }

    pub fn set_candidate_review(&self, id: &str, class: Option<&str>, floor_risk: Option<&str>, llm_risk: Option<&str>, review: &serde_json::Value) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE candidate SET class = COALESCE(?2, class), floor_risk = COALESCE(?3, floor_risk),
                        llm_risk = COALESCE(?4, llm_risk), review = ?5 WHERE id = ?1",
                params![id, class, floor_risk, llm_risk, review.to_string()],
            )?;
            Ok(())
        })
    }

    /// Move a candidate into a new run (approved-but-unpublished carry-over).
    pub fn adopt_candidate(&self, id: &str, run_id: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE candidate SET run_id = ?2 WHERE id = ?1",
                params![id, run_id],
            )?;
            Ok(())
        })
    }

    /// Close open candidates of a package for commits other than `keep`.
    pub fn supersede_candidates(&self, pkgbase: &str, keep_commit: &str) -> Result<Vec<String>> {
        self.with(|c| {
            let placeholders = OPEN_STATES.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut st = c.prepare(&format!(
                "UPDATE candidate SET state = 'superseded', decided_at = ?
                 WHERE pkgbase = ? AND kind IN ('update', 'onboarding') AND aur_commit != ? AND state IN ({placeholders})
                 RETURNING id"
            ))?;
            let now = clock::now();
            let mut p: Vec<&dyn rusqlite::ToSql> = vec![&now, &pkgbase, &keep_commit];
            for s in OPEN_STATES {
                p.push(s);
            }
            Ok(st
                .query_map(p.as_slice(), |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Every candidate of a package, newest first.
    pub fn candidates_of(&self, pkgbase: &str) -> Result<Vec<CandidateRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {CAND_COLS} FROM candidate WHERE pkgbase = ?1 ORDER BY first_seen_at DESC, id DESC"
            ))?;
            Ok(st.query_map(params![pkgbase], row_to_candidate)?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn jobs_for_candidate(&self, candidate_id: &str) -> Result<Vec<crate::JobRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {} FROM job WHERE candidate_id = ?1 ORDER BY queued_at",
                crate::JOB_COLS
            ))?;
            Ok(st
                .query_map(params![candidate_id], crate::row_to_job)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_and_candidate_lifecycle() {
        let s = Store::open_memory().unwrap();
        assert!(s.active_run().unwrap().is_none());
        let run = s.create_run().unwrap();
        assert_eq!(s.active_run().unwrap().unwrap().id, run.id);
        s.set_run_step(&run.id, "running", Some("poll")).unwrap();

        let c = s
            .create_candidate(
                &run.id,
                &NewCandidate {
                    pkgbase: "foo",
                    kind: "update",
                    aur_commit: Some("c1"),
                    first_seen_last_modified: Some(1234),
                    state: "pending",
                    review: serde_json::json!({}),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(s.open_candidates("foo").unwrap().len(), 1);
        s.set_candidate_review(&c.id, Some("routine"), Some("low"), None, &serde_json::json!({"changes": 2}))
            .unwrap();
        assert!(s.decide_candidate(&c.id, "approved", "ben", Some("ok")).unwrap());
        let got = s.get_candidate(&c.id).unwrap().unwrap();
        assert_eq!(got.state, "approved");
        assert_eq!(got.class.as_deref(), Some("routine"));
        assert_eq!(got.review["changes"], 2);
        assert_eq!(s.candidates_in_states(&["approved"]).unwrap().len(), 1);

        // A new head supersedes the open candidate.
        let superseded = s.supersede_candidates("foo", "c2").unwrap();
        assert_eq!(superseded, vec![c.id.clone()]);
        assert!(s.open_candidates("foo").unwrap().is_empty());

        let r = s
            .create_candidate(
                &run.id,
                &NewCandidate {
                    pkgbase: "foo",
                    kind: "update",
                    aur_commit: Some("c2"),
                    state: "pending",
                    ..Default::default()
                },
            )
            .unwrap();
        s.decide_candidate(&r.id, "rejected", "ben", None).unwrap();
        assert!(s.rejected_candidate_for("foo", "c2").unwrap().is_some());
        assert!(s.rejected_candidate_for("foo", "c3").unwrap().is_none());

        s.finish_run(&run.id, "done", &serde_json::json!({"built": 0})).unwrap();
        assert!(s.active_run().unwrap().is_none());
        assert_eq!(s.list_runs(10).unwrap()[0].summary["built"], 0);
    }
}
