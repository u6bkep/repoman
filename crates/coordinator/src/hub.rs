//! Worker connections and job dispatch.
//!
//! Each connected worker has a writer task fed by an unbounded channel, and a
//! reader loop that applies its messages to the store. A single dispatcher
//! task assigns queued jobs to workers with free slots whenever it is woken
//! (new job, job finished, worker joined) or every few seconds as a fallback.
//! Connections are identified by a counter so that a stale connection's
//! cleanup never removes its replacement.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use repoman_common::config::CoordinatorConfig;
use repoman_common::proto::{CoordMsg, JobId, JobSpec, WorkerMsg};
use repoman_store::Store;
use serde::Serialize;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const DISPATCH_TICK: Duration = Duration::from_secs(5);

pub struct Hub {
    store: Arc<Store>,
    credentials: HashMap<String, String>,
    orphan_grace_secs: u64,
    connected: Mutex<HashMap<String, Connected>>,
    wake: Notify,
    next_conn_id: AtomicU64,
    done: tokio::sync::broadcast::Sender<JobId>,
}

struct Connected {
    conn_id: u64,
    tx: mpsc::UnboundedSender<CoordMsg>,
    arch: String,
    slots: u32,
    capabilities: Vec<String>,
    /// Running jobs with the memory reserved for each (0 when unknown).
    running: HashMap<JobId, u64>,
    mem_bytes: Option<u64>,
    since: String,
}

/// Memory a worker keeps for itself and the host: a tenth, at least 1 GiB.
fn host_reserve(mem: u64) -> u64 {
    (mem / 10).max(1 << 30)
}

/// Can a worker with `mem` bytes, `reserved` already promised to `running`
/// jobs, take a job needing `needed`? A job too big for the machine still
/// runs when nothing else does, alone, rather than never.
pub fn fits(mem: Option<u64>, reserved: u64, running: usize, needed: u64) -> bool {
    match mem {
        None => true,
        Some(m) => {
            let avail = m.saturating_sub(host_reserve(m)).saturating_sub(reserved);
            avail >= needed || running == 0
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectedWorker {
    pub name: String,
    pub arch: String,
    pub slots: u32,
    pub capabilities: Vec<String>,
    pub running: Vec<JobId>,
    pub since: String,
}

impl Hub {
    pub fn new(store: Arc<Store>, config: &CoordinatorConfig) -> Arc<Self> {
        Arc::new(Hub {
            store,
            credentials: config
                .workers
                .iter()
                .map(|w| (w.name.clone(), w.token.clone()))
                .collect(),
            orphan_grace_secs: config.orphan_grace_secs,
            connected: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            next_conn_id: AtomicU64::new(1),
            done: tokio::sync::broadcast::channel(1024).0,
        })
    }

    /// Wake the dispatcher, e.g. after a job was created.
    pub fn notify(&self) {
        self.wake.notify_one();
    }

    /// Tell whichever worker holds each job to stop it, and free its slot.
    pub fn cancel_jobs(&self, ids: &[JobId]) {
        let mut map = self.connected.lock().unwrap();
        for c in map.values_mut() {
            for id in ids {
                if c.running.remove(id).is_some() {
                    let _ = c.tx.send(CoordMsg::Cancel { job_id: id.clone() });
                }
            }
        }
    }

    /// Receive the id of every job that reaches a terminal state.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<JobId> {
        self.done.subscribe()
    }

    pub fn connected_workers(&self) -> Vec<ConnectedWorker> {
        let mut out: Vec<_> = self
            .connected
            .lock()
            .unwrap()
            .iter()
            .map(|(name, c)| ConnectedWorker {
                name: name.clone(),
                arch: c.arch.clone(),
                slots: c.slots,
                capabilities: c.capabilities.clone(),
                running: c.running.keys().cloned().collect(),
                since: c.since.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    // ---- dispatch ---------------------------------------------------------

    pub async fn dispatcher(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(DISPATCH_TICK) => {}
            }
            if let Err(e) = self.dispatch_once() {
                tracing::error!(error = %e, "dispatch failed");
            }
        }
    }

    fn dispatch_once(&self) -> anyhow::Result<()> {
        for id in self.store.requeue_stale_orphans(self.orphan_grace_secs)? {
            tracing::warn!(job = %id, "orphan grace expired, requeued");
            self.store.event("job.requeued", Some(&id), serde_json::json!({"reason": "orphan_grace"}))?;
        }
        for job in self.store.queued_jobs()? {
            let Some((worker, tx)) = self.reserve_slot(&job.id, required_capability(&job.job), job.job.memory_limit().unwrap_or(0)) else {
                continue; // no connected worker with a free slot and enough memory can run this kind
            };
            if !self.store.set_assigned(&job.id, &worker)? {
                self.release_slot(&worker, &job.id);
                continue; // state changed under us; skip
            }
            let spec = JobSpec {
                id: job.id.clone(),
                job: job.job.clone(),
                pkgbase: job.pkgbase.clone(),
                stream: job.stream.clone(),
            };
            if tx.send(CoordMsg::Assign { spec }).is_err() {
                tracing::warn!(job = %job.id, %worker, "worker channel closed during assign");
                self.release_slot(&worker, &job.id);
                self.store.requeue(&job.id)?;
                continue;
            }
            tracing::info!(job = %job.id, %worker, kind = job.job.name(), "assigned");
            self.store
                .event("job.assigned", Some(&job.id), serde_json::json!({"worker": worker}))?;
        }
        Ok(())
    }

    /// Pick the capable connected worker with a free slot and room for
    /// `needed` bytes beside what its running jobs were promised; prefer the
    /// one with the most memory left, then the most free slots.
    fn reserve_slot(&self, job: &JobId, capability: Option<&str>, needed: u64) -> Option<(String, mpsc::UnboundedSender<CoordMsg>)> {
        let mut map = self.connected.lock().unwrap();
        let (name, c) = map
            .iter_mut()
            .filter(|(_, c)| (c.running.len() as u32) < c.slots)
            .filter(|(_, c)| capability.is_none_or(|cap| c.capabilities.iter().any(|x| x == cap)))
            .filter(|(_, c)| fits(c.mem_bytes, c.running.values().sum(), c.running.len(), needed))
            .max_by_key(|(_, c)| {
                let left = c.mem_bytes.map(|m| m.saturating_sub(c.running.values().sum::<u64>())).unwrap_or(u64::MAX);
                (left, c.slots - c.running.len() as u32)
            })?;
        c.running.insert(job.clone(), needed);
        Some((name.clone(), c.tx.clone()))
    }

    fn release_slot(&self, worker: &str, job: &JobId) {
        if let Some(c) = self.connected.lock().unwrap().get_mut(worker) {
            c.running.remove(job);
        }
    }

    // ---- connections ------------------------------------------------------

    pub async fn handle_socket(self: Arc<Self>, socket: WebSocket) {
        let (mut sink, mut stream) = socket.split();

        // The first frame must be a Hello.
        let hello = match timeout(HELLO_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<WorkerMsg>(t.as_str()),
            Ok(Some(Ok(_))) | Ok(None) | Ok(Some(Err(_))) => {
                tracing::warn!("worker connection closed before hello");
                return;
            }
            Err(_) => {
                tracing::warn!("worker connection timed out before hello");
                return;
            }
        };
        let (name, token, arch, slots, version, mem_bytes, scratch_bytes, in_flight, capabilities) = match hello {
            Ok(WorkerMsg::Hello {
                name,
                token,
                arch,
                slots,
                version,
                mem_bytes,
                scratch_bytes,
                in_flight,
                capabilities,
            }) => (name, token, arch, slots, version, mem_bytes, scratch_bytes, in_flight, capabilities),
            Ok(other) => {
                tracing::warn!(?other, "first worker message was not hello");
                let _ = send(&mut sink, &CoordMsg::Rejected { reason: "expected hello".into() }).await;
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "unparseable hello");
                let _ = send(&mut sink, &CoordMsg::Rejected { reason: format!("bad hello: {e}") }).await;
                return;
            }
        };

        if self.credentials.get(&name) != Some(&token) {
            tracing::warn!(worker = %name, "rejected: bad credentials");
            let _ = send(&mut sink, &CoordMsg::Rejected { reason: "bad credentials".into() }).await;
            return;
        }

        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = self
            .store
            .upsert_worker(&name, &arch, slots, mem_bytes, scratch_bytes)
        {
            tracing::error!(error = %e, "upsert worker");
            return;
        }
        let rec = match self.store.reconcile_worker(&name, &in_flight) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "reconcile worker");
                return;
            }
        };
        for id in &rec.requeued {
            tracing::warn!(worker = %name, job = %id, "worker lost job, requeued");
        }
        for id in &rec.conflicts {
            tracing::error!(worker = %name, job = %id, "worker claims a job held by another worker");
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<CoordMsg>();
        {
            let mut map = self.connected.lock().unwrap();
            if let Some(old) = map.insert(
                name.clone(),
                Connected {
                    conn_id,
                    tx: tx.clone(),
                    arch: arch.clone(),
                    slots,
                    capabilities: capabilities.clone(),
                    running: rec.resumed.iter().cloned().map(|j| (j, 0)).collect(),
                    mem_bytes,
                    since: repoman_common::clock::now(),
                },
            ) {
                tracing::warn!(worker = %name, old_conn = old.conn_id, "replaced existing connection");
            }
        }
        tracing::info!(worker = %name, %arch, slots, %version, ?capabilities, resumed = rec.resumed.len(), "worker connected");
        let _ = self.store.event(
            "worker.connected",
            Some(&name),
            serde_json::json!({"arch": arch, "slots": slots, "version": version, "resumed": rec.resumed}),
        );
        let _ = tx.send(CoordMsg::Welcome {
            worker: name.clone(),
            resumed: rec.resumed.clone(),
        });
        self.notify();

        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if send(&mut sink, &msg).await.is_err() {
                    break;
                }
            }
        });

        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(t)) => match serde_json::from_str::<WorkerMsg>(t.as_str()) {
                    Ok(msg) => {
                        if let Err(e) = self.on_message(&name, &tx, msg) {
                            tracing::error!(worker = %name, error = %e, "handling worker message");
                        }
                    }
                    Err(e) => tracing::warn!(worker = %name, error = %e, "unparseable worker message"),
                },
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {} // ping/pong/binary: ignore
            }
        }

        writer.abort();
        let still_current = {
            let mut map = self.connected.lock().unwrap();
            match map.get(&name) {
                Some(c) if c.conn_id == conn_id => {
                    map.remove(&name);
                    true
                }
                _ => false,
            }
        };
        if still_current {
            match self.store.orphan_worker_jobs(&name) {
                Ok(ids) => {
                    tracing::warn!(worker = %name, orphaned = ids.len(), "worker disconnected");
                    let _ = self.store.event(
                        "worker.disconnected",
                        Some(&name),
                        serde_json::json!({"orphaned": ids}),
                    );
                }
                Err(e) => tracing::error!(error = %e, "orphaning jobs"),
            }
        } else {
            tracing::info!(worker = %name, conn_id, "stale connection closed");
        }
    }

    fn on_message(
        &self,
        worker: &str,
        tx: &mpsc::UnboundedSender<CoordMsg>,
        msg: WorkerMsg,
    ) -> anyhow::Result<()> {
        match msg {
            WorkerMsg::Hello { .. } => tracing::warn!(%worker, "duplicate hello ignored"),
            WorkerMsg::Accepted { job_id } => {
                if !self.store.set_running(&job_id, worker)? {
                    tracing::warn!(%worker, job = %job_id, "accepted for a job not assigned to it");
                }
            }
            WorkerMsg::StageStarted { job_id, stage, ts } => {
                self.store.stage_started(&job_id, &stage, &ts)?;
            }
            WorkerMsg::StageFinished { job_id, stage, ts } => {
                self.store.stage_finished(&job_id, &stage, &ts)?;
            }
            WorkerMsg::Log { job_id, lines } => {
                self.store.append_logs(&job_id, &lines)?;
            }
            WorkerMsg::Finished { job_id, outcome } => {
                let applied = self.store.finish_job(&job_id, worker, &outcome)?;
                self.release_slot(worker, &job_id);
                let _ = self.done.send(job_id.clone());
                if applied {
                    tracing::info!(%worker, job = %job_id, exit = outcome.exit_code, "job finished");
                    self.store.event(
                        "job.finished",
                        Some(&job_id),
                        serde_json::json!({"worker": worker, "exit_code": outcome.exit_code, "error": outcome.error}),
                    )?;
                } else {
                    tracing::warn!(%worker, job = %job_id, "finish ignored: job not held by this worker");
                }
                self.notify();
            }
            WorkerMsg::Heartbeat => {
                self.store.touch_worker(worker)?;
                let _ = tx.send(CoordMsg::Pong);
            }
        }
        Ok(())
    }
}

async fn send<S>(sink: &mut S, msg: &CoordMsg) -> Result<(), ()>
where
    S: SinkExt<Message> + Unpin,
{
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sink.send(Message::Text(text.into())).await.map_err(|_| ())
}

/// Worker capability a job kind needs, if any.
pub fn required_capability(job: &repoman_common::proto::JobKind) -> Option<&'static str> {
    use repoman_common::proto::JobKind;
    match job {
        JobKind::Noop { .. } => None,
        JobKind::Image { .. } => Some("image"),
        JobKind::Build { .. } | JobKind::Inspect { .. } => Some("build"),
        JobKind::Probe { .. } => Some("probe"),
    }
}

#[cfg(test)]
mod admission_tests {
    use super::fits;

    #[test]
    fn memory_admission() {
        let gib = 1u64 << 30;
        assert!(fits(None, 0, 0, 100 * gib), "unknown memory never blocks");
        // 64 GiB machine keeps 6.4 GiB; two 4 GiB jobs leave room for a third 4 GiB job.
        assert!(fits(Some(64 * gib), 8 * gib, 2, 4 * gib));
        // but not for a 50 GiB one...
        assert!(!fits(Some(64 * gib), 8 * gib, 2, 50 * gib));
        // ...which runs alone.
        assert!(fits(Some(64 * gib), 0, 0, 50 * gib));
        assert!(fits(Some(8 * gib), 0, 0, 100 * gib), "oversized job still runs on an idle worker");
        assert!(!fits(Some(8 * gib), 4 * gib, 1, 4 * gib), "8 GiB minus 1 GiB reserve minus 4 reserved leaves 3");
    }
}
