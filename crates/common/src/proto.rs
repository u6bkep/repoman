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

/// A base image the worker must have unpacked before a job can run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRef {
    pub sha256: String,
    /// Where to download the tarball if it is not present locally.
    pub url: String,
}

/// What a job does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// Exercises the protocol without touching the system.
    Noop {
        #[serde(default)]
        duration_ms: u64,
        #[serde(default)]
        fail: bool,
    },
    /// Build a base image for a stream with pacstrap and upload it.
    Image {
        stream: String,
        /// Full pacman.conf text for the stream's repos.
        pacman_conf: String,
        packages: Vec<String>,
        /// Keyrings to populate inside the image, e.g. `archlinux`, `manjaro`.
        keyrings: Vec<String>,
        /// Appended as `/etc/makepkg.conf.d/repoman.conf` inside the image.
        makepkg_conf_extra: String,
        /// `PUT {upload_url}/{sha256}?name=...` with the worker's bearer token.
        upload_url: String,
    },
    /// Build one package base for one stream.
    Build {
        pkgbase: String,
        stream: String,
        aur_commit: String,
        image: ImageRef,
        /// Tarball of the AUR repo at `aur_commit`.
        source_url: String,
        /// pacman.conf for the build root: stream repos plus staging repo.
        pacman_conf: String,
        /// Package names to install before building: the resolved depends,
        /// makedepends, and checkdepends.
        install: Vec<String>,
        /// Armored public keys for `validpgpkeys`.
        #[serde(default)]
        pgp_keys: Vec<String>,
        /// Skip signature verification of sources (per-package policy).
        #[serde(default)]
        skip_pgp: bool,
        /// Leave the network up during build(): for packages whose prepare()
        /// fetches at build time. Per-package policy, off by default.
        #[serde(default)]
        allow_network: bool,
        upload_url: String,
        /// cgroup memory.max for this build, predicted by the coordinator
        /// from recorded peaks; the worker's configured default when unset.
        #[serde(default)]
        memory_limit: Option<u64>,
    },
    /// Regenerate SRCINFO from the PKGBUILD in a sandbox.
    Inspect {
        pkgbase: String,
        aur_commit: String,
        image: ImageRef,
        source_url: String,
    },
    /// Query upstream VCS heads for a package's sources.
    Probe {
        pkgbase: String,
        /// makepkg `source=` entries that are VCS fetches.
        sources: Vec<String>,
    },
}

impl JobKind {
    /// Memory the coordinator predicted for this job, if it is a build.
    pub fn memory_limit(&self) -> Option<u64> {
        match self {
            JobKind::Build { memory_limit, .. } => *memory_limit,
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            JobKind::Noop { .. } => "noop",
            JobKind::Image { .. } => "image",
            JobKind::Build { .. } => "build",
            JobKind::Inspect { .. } => "inspect",
            JobKind::Probe { .. } => "probe",
        }
    }

    pub fn image(&self) -> Option<&ImageRef> {
        match self {
            JobKind::Build { image, .. } | JobKind::Inspect { image, .. } => Some(image),
            _ => None,
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Peaks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_scratch_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<f64>,
}

/// One built package file, already uploaded to the coordinator's store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub sha256: String,
    pub filename: String,
    pub size: u64,
    pub pkgname: String,
    pub pkgbase: String,
    pub pkgver: String,
    pub arch: String,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    /// Payload paths, for the artifact diff.
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub has_install_script: bool,
    /// Payload files with the setuid or setgid bit.
    #[serde(default)]
    pub setuid_paths: Vec<String>,
    /// `.BUILDINFO` key/values as JSON.
    #[serde(default)]
    pub buildinfo: serde_json::Value,
}

/// Job-kind specific output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobResult {
    Image {
        sha256: String,
        size: u64,
        filename: String,
        /// Installed `name version` lines inside the image.
        packages: Vec<String>,
    },
    Build {
        artifacts: Vec<ArtifactInfo>,
        #[serde(default)]
        net_attempts: u64,
        #[serde(default)]
        outside_writes: Vec<String>,
        /// Revision actually built for VCS packages, from pkgver().
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upstream_rev: Option<String>,
    },
    Inspect {
        srcinfo: String,
    },
    Probe {
        /// (source entry, upstream revision)
        revisions: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub peaks: Peaks,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
}

impl Outcome {
    pub fn success() -> Self {
        Outcome {
            exit_code: 0,
            error: None,
            peaks: Peaks::default(),
            result: None,
        }
    }

    pub fn success_with(result: JobResult) -> Self {
        Outcome {
            result: Some(result),
            ..Outcome::success()
        }
    }

    pub fn failure(exit_code: i32, error: impl Into<String>) -> Self {
        Outcome {
            exit_code,
            error: Some(error.into()),
            peaks: Peaks::default(),
            result: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.exit_code == 0 && self.error.is_none()
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
        /// Capabilities: `image` means it can run pacstrap, `build` means it
        /// can run the privileged executor.
        #[serde(default)]
        capabilities: Vec<String>,
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
    fn outcome_with_result_round_trips() {
        let o = Outcome::success_with(JobResult::Probe {
            revisions: vec![("git+https://x".into(), "abc".into())],
        });
        let s = serde_json::to_string(&o).unwrap();
        let back: Outcome = serde_json::from_str(&s).unwrap();
        assert_eq!(back, o);
        assert!(back.is_success());
        // Older messages without `result` still parse.
        let legacy: Outcome = serde_json::from_str(r#"{"exit_code":0}"#).unwrap();
        assert!(legacy.result.is_none());
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
