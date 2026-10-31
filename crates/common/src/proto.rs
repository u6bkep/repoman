//! Wire protocol between coordinator and worker. JSON over WebSocket, one
//! message per text frame. The worker opens the connection and speaks first.
//!
//! Job lifecycle as seen from the coordinator:
//!
//! ```text
//! queued ─assign─► assigned ─Accepted─► running ─Finished─► done | failed
//!                     │                    │
//!                     └── worker gone ─────┴──► orphaned ─┬─ worker returns with job ─► running
//!                                                         └─ grace expires ──────────► queued
//! ```

use serde::{Deserialize, Serialize};

pub type JobId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Assigned,
    Running,
    Orphaned,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Assigned => "assigned",
            JobState::Running => "running",
            JobState::Orphaned => "orphaned",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => JobState::Queued,
            "assigned" => JobState::Assigned,
            "running" => JobState::Running,
            "orphaned" => JobState::Orphaned,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            "cancelled" => JobState::Cancelled,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, JobState::Done | JobState::Failed | JobState::Cancelled)
    }

    /// States in which a worker is, or was, responsible for the job.
    pub fn is_held(self) -> bool {
        matches!(self, JobState::Assigned | JobState::Running | JobState::Orphaned)
    }
}

/// What a job does. M0 has only the no-op used to exercise the protocol;
/// `build`, `inspect`, and `probe` arrive with M1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    Noop {
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        fail: bool,
    },
}

impl JobKind {
    pub fn name(&self) -> &'static str {
        match self {
            JobKind::Noop { .. } => "noop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: JobId,
    pub job: JobKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkgbase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fd {
    Stdout,
    Stderr,
    /// Lines produced by the executor itself rather than the job's processes.
    System,
}

impl Fd {
    pub fn as_str(self) -> &'static str {
        match self {
            Fd::Stdout => "stdout",
            Fd::Stderr => "stderr",
            Fd::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "stdout" => Fd::Stdout,
            "stderr" => Fd::Stderr,
            "system" => Fd::System,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: String,
    pub fd: Fd,
    pub line: String,
}

/// Resource peaks measured by the worker's cgroup and scratch accounting.
/// All optional: the no-op job measures nothing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Peaks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_scratch_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub peaks: Peaks,
}

impl Outcome {
    pub fn success() -> Self {
        Outcome {
            exit_code: 0,
            error: None,
            peaks: Peaks::default(),
        }
    }

    pub fn failure(exit_code: i32, error: impl Into<String>) -> Self {
        Outcome {
            exit_code,
            error: Some(error.into()),
            peaks: Peaks::default(),
        }
    }
}

/// Worker → coordinator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMsg {
    /// First message on every connection. `in_flight` lists jobs the worker
    /// is still executing from a previous connection, so the coordinator can
    /// resume them instead of requeueing.
    Hello {
        name: String,
        token: String,
        arch: String,
        slots: u32,
        version: String,
        #[serde(default)]
        mem_bytes: Option<u64>,
        #[serde(default)]
        scratch_bytes: Option<u64>,
        #[serde(default)]
        in_flight: Vec<JobId>,
    },
    Accepted {
        job_id: JobId,
    },
    StageStarted {
        job_id: JobId,
        stage: String,
        ts: String,
    },
    StageFinished {
        job_id: JobId,
        stage: String,
        ts: String,
    },
    Log {
        job_id: JobId,
        lines: Vec<LogLine>,
    },
    Finished {
        job_id: JobId,
        outcome: Outcome,
    },
    Heartbeat,
}

/// Coordinator → worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoordMsg {
    Welcome {
        worker: String,
        /// Jobs from `in_flight` the coordinator agreed to resume.
        #[serde(default)]
        resumed: Vec<JobId>,
    },
    Rejected {
        reason: String,
    },
    Assign {
        spec: JobSpec,
    },
    Cancel {
        job_id: JobId,
    },
    Pong,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_kind_tagging() {
        let k = JobKind::Noop {
            duration_ms: 5,
            fail: false,
        };
        let s = serde_json::to_string(&k).unwrap();
        assert_eq!(s, r#"{"kind":"noop","duration_ms":5,"fail":false}"#);
        assert_eq!(serde_json::from_str::<JobKind>(&s).unwrap(), k);
    }

    #[test]
    fn message_tagging() {
        let m = WorkerMsg::Accepted {
            job_id: "x".into(),
        };
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"type":"accepted","job_id":"x"}"#
        );
    }

    #[test]
    fn state_strings_round_trip() {
        for s in [
            JobState::Queued,
            JobState::Assigned,
            JobState::Running,
            JobState::Orphaned,
            JobState::Done,
            JobState::Failed,
            JobState::Cancelled,
        ] {
            assert_eq!(JobState::parse(s.as_str()), Some(s));
        }
    }
}
