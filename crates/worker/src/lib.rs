//! The worker: holds one WebSocket to the coordinator, executes assigned
//! jobs, and reports progress. Outbound messages go through a channel owned
//! by `WorkerState`, not by the connection, so a job that outlives a dropped
//! connection keeps reporting and its messages are delivered, in order, once
//! the worker reconnects and re-announces the job in `Hello.in_flight`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use repoman_common::config::WorkerConfig;
use repoman_common::proto::{CoordMsg, JobId, WorkerMsg};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::{interval, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

pub mod build;
pub mod cgroup;
pub mod exec;
pub mod image;
pub mod logs;
pub mod probe;
pub mod sandbox;
pub mod upload;

const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub struct WorkerState {
    pub tx: mpsc::UnboundedSender<WorkerMsg>,
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<WorkerMsg>>,
    /// A message taken from `rx` whose send failed; re-sent first next session
    /// so ordering is preserved.
    pending: Mutex<Option<WorkerMsg>>,
    in_flight: Mutex<HashSet<JobId>>,
    pub slots: Arc<Semaphore>,
    pub cgroups: cgroup::CgroupManager,
    pub capabilities: Vec<String>,
    pub runner: Arc<dyn exec::JobRunner>,
    /// Serialises pacman runs that write the shared package cache: two
    /// instances downloading the same file into one cache corrupt it.
    pub pacman_lock: tokio::sync::Mutex<()>,
    /// Serialises image download and unpack across concurrent jobs.
    pub image_lock: tokio::sync::Mutex<()>,
}

impl WorkerState {
    /// State for tests and unprivileged use: no cgroups, no sandbox.
    pub fn new(slots: u32) -> Arc<Self> {
        Self::with_capabilities(slots, cgroup::CgroupManager::disabled(), Vec::new(), Arc::new(exec::RealRunner))
    }

    /// State with a custom job runner, for integration tests that fake the
    /// privileged executor.
    pub fn with_runner(slots: u32, capabilities: Vec<String>, runner: Arc<dyn exec::JobRunner>) -> Arc<Self> {
        Self::with_capabilities(slots, cgroup::CgroupManager::disabled(), capabilities, runner)
    }

    /// Detect what this process can do and set up cgroups accordingly.
    pub fn detect(slots: u32) -> Arc<Self> {
        let mut caps = Vec::new();
        match sandbox::available() {
            Ok(()) => {
                caps.push("build".to_string());
                if sandbox::which("pacman").is_some() {
                    caps.push("image".to_string());
                }
            }
            Err(why) => tracing::warn!(%why, "sandbox unavailable; this worker will only run noop and probe jobs"),
        }
        caps.push("probe".to_string());
        let cgroups = if caps.iter().any(|c| c == "build") {
            cgroup::CgroupManager::init()
        } else {
            cgroup::CgroupManager::disabled()
        };
        Self::with_capabilities(slots, cgroups, caps, Arc::new(exec::RealRunner))
    }

    fn with_capabilities(slots: u32, cgroups: cgroup::CgroupManager, capabilities: Vec<String>, runner: Arc<dyn exec::JobRunner>) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(WorkerState {
            tx,
            rx: tokio::sync::Mutex::new(rx),
            pending: Mutex::new(None),
            in_flight: Mutex::new(HashSet::new()),
            slots: Arc::new(Semaphore::new(slots as usize)),
            cgroups,
            capabilities,
            runner,
            pacman_lock: tokio::sync::Mutex::new(()),
            image_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn in_flight(&self) -> Vec<JobId> {
        let mut v: Vec<_> = self.in_flight.lock().unwrap().iter().cloned().collect();
        v.sort();
        v
    }

    fn start_job(&self, id: &JobId) -> bool {
        self.in_flight.lock().unwrap().insert(id.clone())
    }

    fn end_job(&self, id: &JobId) {
        self.in_flight.lock().unwrap().remove(id);
    }
}

pub async fn run(config: WorkerConfig) -> Result<()> {
    let state = WorkerState::detect(config.slots);
    tracing::info!(capabilities = ?state.capabilities, "worker starting");
    run_with_state(config, state).await
}

/// Reconnect forever with exponential backoff. Returns only on a credential
/// rejection, which no amount of retrying will fix.
pub async fn run_with_state(config: WorkerConfig, state: Arc<WorkerState>) -> Result<()> {
    std::fs::create_dir_all(&config.scratch_dir)
        .with_context(|| format!("creating {}", config.scratch_dir.display()))?;
    image::prune_scratch(&config.scratch_dir, 14, 3);
    let config = Arc::new(config);
    let mut backoff = Duration::from_secs(1);
    loop {
        match session(&config, &state).await {
            Ok(()) => {
                tracing::warn!("connection closed; reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(SessionError::Rejected(reason)) => {
                bail!("coordinator rejected this worker: {reason}");
            }
            Err(SessionError::Other(e)) => {
                tracing::warn!(error = %e, backoff = ?backoff, "session failed; reconnecting");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

enum SessionError {
    Rejected(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for SessionError {
    fn from(e: anyhow::Error) -> Self {
        SessionError::Other(e)
    }
}

async fn session(config: &Arc<WorkerConfig>, state: &Arc<WorkerState>) -> Result<(), SessionError> {
    let (ws, _) = connect_async(config.coordinator.as_str())
        .await
        .with_context(|| format!("connecting to {}", config.coordinator))?;
    let (mut sink, mut stream) = ws.split();

    let hello = WorkerMsg::Hello {
        name: config.name.clone(),
        token: config.token.clone(),
        arch: config.arch.clone(),
        slots: config.slots,
        version: repoman_common::VERSION.to_string(),
        mem_bytes: total_memory(),
        scratch_bytes: None,
        in_flight: state.in_flight(),
        capabilities: state.capabilities.clone(),
    };
    send(&mut sink, &hello).await?;

    match timeout(WELCOME_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str::<CoordMsg>(t.as_str()) {
            Ok(CoordMsg::Welcome { resumed, .. }) => {
                tracing::info!(coordinator = %config.coordinator, resumed = resumed.len(), "connected");
            }
            Ok(CoordMsg::Rejected { reason }) => return Err(SessionError::Rejected(reason)),
            Ok(other) => return Err(anyhow::anyhow!("unexpected first message: {other:?}").into()),
            Err(e) => return Err(anyhow::anyhow!("unparseable welcome: {e}").into()),
        },
        Ok(_) => return Err(anyhow::anyhow!("connection closed before welcome").into()),
        Err(_) => return Err(anyhow::anyhow!("timed out waiting for welcome").into()),
    }

    // Flush a message whose send failed on the previous session before
    // draining the channel, so the coordinator sees events in order.
    let pending = state.pending.lock().unwrap().take();
    if let Some(msg) = pending {
        if let Err(e) = send(&mut sink, &msg).await {
            *state.pending.lock().unwrap() = Some(msg);
            return Err(e.into());
        }
    }

    let mut rx = state.rx.lock().await;
    let mut heartbeat = interval(HEARTBEAT);
    heartbeat.tick().await; // first tick fires immediately; skip it

    loop {
        tokio::select! {
            out = rx.recv() => {
                let msg = out.expect("worker channel never closes while state lives");
                if let Err(e) = send(&mut sink, &msg).await {
                    *state.pending.lock().unwrap() = Some(msg);
                    return Err(e.into());
                }
            }
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<CoordMsg>(t.as_str()) {
                    Ok(msg) => handle(config, state, msg),
                    Err(e) => tracing::warn!(error = %e, "unparseable coordinator message"),
                },
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(e)) => return Err(anyhow::anyhow!(e).into()),
                Some(Ok(_)) => {}
            },
            _ = heartbeat.tick() => {
                let _ = state.tx.send(WorkerMsg::Heartbeat);
            }
        }
    }
}

fn handle(config: &Arc<WorkerConfig>, state: &Arc<WorkerState>, msg: CoordMsg) {
    match msg {
        CoordMsg::Assign { spec } => {
            if !state.start_job(&spec.id) {
                tracing::warn!(job = %spec.id, "duplicate assignment ignored");
                return;
            }
            tracing::info!(job = %spec.id, kind = spec.job.name(), "assigned");
            let state = state.clone();
            let config = config.clone();
            tokio::spawn(async move {
                let id = spec.id.clone();
                exec::run_job(spec, config, state.clone()).await;
                state.end_job(&id);
            });
        }
        CoordMsg::Cancel { job_id } => {
            tracing::warn!(job = %job_id, "cancel requested; not supported yet");
        }
        CoordMsg::Pong => {}
        CoordMsg::Welcome { .. } | CoordMsg::Rejected { .. } => {
            tracing::warn!(?msg, "unexpected message after handshake");
        }
    }
}

async fn send<S>(sink: &mut S, msg: &WorkerMsg) -> Result<()>
where
    S: SinkExt<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let text = serde_json::to_string(msg)?;
    sink.send(Message::text(text)).await.context("sending frame")
}

fn total_memory() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = s.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}
