//! Job execution: dispatch by kind, shared reporting context, process
//! supervision with timeouts and log capture.
//!
//! The dispatch itself sits behind the `JobRunner` trait so integration
//! tests can drive the coordinator with a worker that fakes images and
//! packages instead of needing root and pacstrap.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use repoman_common::clock;
use repoman_common::config::WorkerConfig;
use repoman_common::proto::{Fd, JobId, JobKind, JobSpec, LogLine, Outcome, Peaks, WorkerMsg};
use tokio::sync::mpsc;

use crate::WorkerState;
use crate::cgroup::JobCgroup;
use crate::logs;

/// Everything a job needs to report progress and find its resources.
pub struct JobCtx {
    pub config: Arc<WorkerConfig>,
    pub http: reqwest::Client,
    state: Arc<WorkerState>,
    pub job_id: JobId,
    cgroup: Mutex<Option<JobCgroup>>,
    scratch_peak: Mutex<u64>,
}

impl JobCtx {
    fn send(&self, msg: WorkerMsg) {
        let _ = self.state.tx.send(msg);
    }

    pub fn job_dir(&self) -> PathBuf {
        self.config.scratch_dir.join("jobs").join(&self.job_id)
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.job_timeout_secs)
    }

    pub fn cgroup_path(&self) -> Option<PathBuf> {
        self.cgroup.lock().unwrap().as_ref().map(|c| c.path.clone())
    }

    pub fn oom_killed(&self) -> bool {
        self.cgroup.lock().unwrap().as_ref().is_some_and(|c| c.oom_killed())
    }

    /// Hold while running pacman against the shared package cache.
    pub async fn lock_pacman(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.state.pacman_lock.lock().await
    }

    /// Hold while downloading or unpacking a base image.
    pub async fn lock_image(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.state.image_lock.lock().await
    }

    pub fn set_scratch_peak(&self, bytes: u64) {
        let mut p = self.scratch_peak.lock().unwrap();
        *p = (*p).max(bytes);
    }

    pub fn stage(&self, stage: &str) {
        self.send(WorkerMsg::StageStarted {
            job_id: self.job_id.clone(),
            stage: stage.to_string(),
            ts: clock::now(),
        });
    }

    pub fn stage_done(&self, stage: &str) {
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
                seq: 0,
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

    fn peaks(&self) -> Peaks {
        let mut p = self
            .cgroup
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.peaks())
            .unwrap_or_default();
        let scratch = *self.scratch_peak.lock().unwrap();
        if scratch > 0 {
            p.peak_scratch_bytes = Some(scratch);
        }
        p
    }
}

pub type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<Outcome>> + Send + 'a>>;

/// Executes one job kind. The real runner shells out to pacstrap, unshare,
/// and makepkg; tests substitute their own.
pub trait JobRunner: Send + Sync + 'static {
    fn run<'a>(&'a self, ctx: &'a JobCtx, spec: &'a JobSpec) -> RunFuture<'a>;
}

pub struct RealRunner;

impl JobRunner for RealRunner {
    fn run<'a>(&'a self, ctx: &'a JobCtx, spec: &'a JobSpec) -> RunFuture<'a> {
        Box::pin(async move {
            match &spec.job {
                JobKind::Noop { duration_ms, fail } => noop(ctx, *duration_ms, *fail).await,
                JobKind::Image {
                    stream,
                    pacman_conf,
                    packages,
                    keyrings,
                    makepkg_conf_extra,
                    upload_url,
                } => crate::image::build_image(ctx, stream, pacman_conf, packages, keyrings, makepkg_conf_extra, upload_url)
                    .await
                    .map(Outcome::success_with),
                JobKind::Build {
                    pkgbase,
                    image,
                    source_url,
                    pacman_conf,
                    install,
                    pgp_keys,
                    skip_pgp,
                    allow_network,
                    upload_url,
                    ..
                } => crate::build::run_build(
                    ctx,
                    crate::build::BuildParams {
                        pkgbase,
                        image,
                        source_url,
                        pacman_conf,
                        install,
                        pgp_keys,
                        skip_pgp: *skip_pgp,
                        allow_network: *allow_network,
                        upload_url,
                        makepkg_conf_extra: "",
                    },
                )
                .await
                .map(Outcome::success_with),
            }
        })
    }
}

/// Run a job to completion and report `Finished`.
pub async fn run_job(spec: JobSpec, config: Arc<WorkerConfig>, state: Arc<WorkerState>) {
    let _permit = state
        .slots
        .acquire()
        .await
        .expect("slot semaphore is never closed");
    let ctx = JobCtx {
        http: reqwest::Client::builder()
            .user_agent(format!("repoman-worker/{}", repoman_common::VERSION))
            .build()
            .expect("http client"),
        cgroup: Mutex::new(state.cgroups.create(&spec.id, spec.job.memory_limit().or(config.job_memory_limit))),
        config,
        state: state.clone(),
        job_id: spec.id.clone(),
        scratch_peak: Mutex::new(0),
    };
    ctx.send(WorkerMsg::Accepted {
        job_id: spec.id.clone(),
    });

    let result = state.runner.run(&ctx, &spec).await;
    let mut outcome = match result {
        Ok(o) => o,
        Err(e) => {
            let msg = format!("{e:#}");
            ctx.log(Fd::System, format!("job failed: {msg}"));
            Outcome::failure(1, msg)
        }
    };
    outcome.peaks = ctx.peaks();

    // Teardown: successful jobs leave nothing; failures keep the job dir
    // for inspection until the worker prunes it.
    let job_dir = ctx.job_dir();
    if job_dir.exists() {
        if outcome.is_success() {
            if let Err(e) = tokio::fs::remove_dir_all(&job_dir).await {
                tracing::warn!(job = %spec.id, error = %e, "removing job dir");
            }
        } else {
            ctx.log(Fd::System, format!("keeping {} for inspection", job_dir.display()));
        }
    }
    if let Some(cg) = ctx.cgroup.lock().unwrap().take() {
        cg.cleanup();
    }

    ctx.send(WorkerMsg::Finished {
        job_id: spec.id.clone(),
        outcome,
    });
}

/// Spawn a command, stream its output as log lines, enforce a timeout.
/// Returns the exit status and the last lines for error messages.
pub async fn run_logged(ctx: &JobCtx, mut cmd: tokio::process::Command, timeout: Option<Duration>) -> Result<(ExitStatus, Vec<String>)> {
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "spawning {}: {e}",
            cmd.as_std().get_program().to_string_lossy()
        )
    })?;
    let (tx, rx) = mpsc::unbounded_channel::<LogLine>();
    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(tokio::spawn(logs::pump(out, Fd::Stdout, tx.clone())));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(tokio::spawn(logs::pump(err, Fd::Stderr, tx.clone())));
    }
    drop(tx);
    let batcher = logs::batch(rx, |lines| ctx.log_lines(lines));

    let wait = async {
        let status = match timeout {
            Some(t) => match tokio::time::timeout(t, child.wait()).await {
                Ok(s) => s?,
                Err(_) => {
                    ctx.log(Fd::System, format!("timed out after {}s; killing", t.as_secs()));
                    let _ = child.kill().await;
                    anyhow::bail!("timed out after {}s", t.as_secs());
                }
            },
            None => child.wait().await?,
        };
        Ok::<ExitStatus, anyhow::Error>(status)
    };
    let (status, tail) = tokio::join!(wait, batcher);
    for p in pumps {
        let _ = p.await;
    }
    Ok((status?, tail))
}

async fn noop(ctx: &JobCtx, duration_ms: u64, fail: bool) -> Result<Outcome> {
    ctx.stage("provision");
    ctx.log(Fd::System, format!("noop job: scratch dir {}", ctx.config.scratch_dir.display()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    ctx.stage_done("provision");

    ctx.stage("build");
    let steps = 4u64;
    for i in 1..=steps {
        tokio::time::sleep(Duration::from_millis(duration_ms / steps)).await;
        ctx.log(Fd::Stdout, format!("step {i}/{steps}"));
    }
    if fail {
        ctx.log(Fd::Stderr, "noop job asked to fail");
    }
    ctx.stage_done("build");

    ctx.stage("teardown");
    ctx.stage_done("teardown");

    if fail {
        Ok(Outcome::failure(1, "noop failure requested"))
    } else {
        Ok(Outcome::success())
    }
}
