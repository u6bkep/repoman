//! Job execution. M0 ships only the no-op job, which walks the same stage
//! and log reporting path the build executor will use in M1.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use repoman_common::clock;
use repoman_common::proto::{Fd, JobId, JobKind, JobSpec, LogLine, Outcome, WorkerMsg};

use crate::WorkerState;

/// Reports one job's progress to the coordinator.
pub struct Reporter<'a> {
    state: &'a WorkerState,
    job_id: JobId,
}

impl<'a> Reporter<'a> {
    fn send(&self, msg: WorkerMsg) {
        // The receiver lives as long as the state; a failure means shutdown.
        let _ = self.state.tx.send(msg);
    }

    pub fn stage_started(&self, stage: &str) {
        self.send(WorkerMsg::StageStarted {
            job_id: self.job_id.clone(),
            stage: stage.to_string(),
            ts: clock::now(),
        });
    }

    pub fn stage_finished(&self, stage: &str) {
        self.send(WorkerMsg::StageFinished {
            job_id: self.job_id.clone(),
            stage: stage.to_string(),
            ts: clock::now(),
        });
    }

    pub fn log(&self, fd: Fd, line: impl Into<String>) {
        self.send(WorkerMsg::Log {
            job_id: self.job_id.clone(),
            lines: vec![LogLine {
                ts: clock::now(),
                fd,
                line: line.into(),
            }],
        });
    }

    pub fn log_lines(&self, lines: Vec<LogLine>) {
        if !lines.is_empty() {
            self.send(WorkerMsg::Log {
                job_id: self.job_id.clone(),
                lines,
            });
        }
    }
}

pub async fn run_job(spec: JobSpec, scratch: &Path, state: &Arc<WorkerState>) {
    let _permit = state
        .slots
        .acquire()
        .await
        .expect("slot semaphore is never closed");
    let rep = Reporter {
        state,
        job_id: spec.id.clone(),
    };
    rep.send(WorkerMsg::Accepted {
        job_id: spec.id.clone(),
    });
    let outcome = match &spec.job {
        JobKind::Noop { duration_ms, fail } => noop(&rep, scratch, *duration_ms, *fail).await,
    };
    rep.send(WorkerMsg::Finished {
        job_id: spec.id.clone(),
        outcome,
    });
}

async fn noop(rep: &Reporter<'_>, scratch: &Path, duration_ms: u64, fail: bool) -> Outcome {
    rep.stage_started("provision");
    rep.log(Fd::System, format!("noop job: scratch dir {}", scratch.display()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    rep.stage_finished("provision");

    rep.stage_started("build");
    let steps = 4u64;
    for i in 1..=steps {
        tokio::time::sleep(Duration::from_millis(duration_ms / steps)).await;
        rep.log(Fd::Stdout, format!("step {i}/{steps}"));
    }
    if fail {
        rep.log(Fd::Stderr, "noop job asked to fail");
    }
    rep.stage_finished("build");

    rep.stage_started("teardown");
    rep.stage_finished("teardown");

    if fail {
        Outcome::failure(1, "noop failure requested")
    } else {
        Outcome::success()
    }
}
