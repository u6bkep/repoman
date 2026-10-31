//! End-to-end: coordinator and worker in one process, talking over a real
//! TCP WebSocket, exercised through the HTTP API.

use std::sync::Arc;
use std::time::{Duration, Instant};

use repoman_common::config::{CoordinatorConfig, WorkerConfig, WorkerCredential};
use repoman_store::Store;
use repoman_worker::WorkerState;
use serde_json::{Value, json};

struct Harness {
    base: String,
    ws: String,
    worker_cfg: WorkerConfig,
    _tmp: tempfile::TempDir,
}

async fn start_coordinator() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = CoordinatorConfig {
        listen: addr.to_string(),
        public_url: format!("http://{addr}"),
        data_dir: tmp.path().join("coord"),
        workers: vec![WorkerCredential {
            name: "w1".into(),
            token: "t1".into(),
        }],
        orphan_grace_secs: 60,
        ..Default::default()
    };
    let store = Arc::new(Store::open(&tmp.path().join("repoman.db")).unwrap());
    tokio::spawn(repoman_coordinator::serve(config, store, listener));
    let worker_cfg = WorkerConfig {
        coordinator: format!("ws://{addr}/ws/worker"),
        name: "w1".into(),
        token: "t1".into(),
        slots: 2,
        scratch_dir: tmp.path().join("scratch"),
        arch: "x86_64".into(),
        ..Default::default()
    };
    Harness {
        base: format!("http://{addr}"),
        ws: format!("ws://{addr}/ws/worker"),
        worker_cfg,
        _tmp: tmp,
    }
}

async fn get(base: &str, path: &str) -> Value {
    reqwest::get(format!("{base}{path}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn post(base: &str, path: &str, body: Value) -> Value {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_for(base: &str, id: &str, pred: impl Fn(&Value) -> bool, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    loop {
        let v = get(base, &format!("/api/jobs/{id}")).await;
        if pred(&v) {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting; last state: {v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_connected(base: &str, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let v = get(base, "/api/workers").await;
        if v.as_array()
            .unwrap()
            .iter()
            .any(|w| w["name"] == name && w["connected"] == true)
        {
            return;
        }
        assert!(Instant::now() < deadline, "worker never connected: {v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn noop_job_round_trips_with_stages_and_logs() {
    let h = start_coordinator().await;
    let worker = tokio::spawn(repoman_worker::run(h.worker_cfg.clone()));
    wait_connected(&h.base, "w1").await;

    let created = post(
        &h.base,
        "/api/jobs",
        json!({"job": {"kind": "noop", "duration_ms": 200}, "pkgbase": "demo"}),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["state"], "queued");

    let done = wait_for(&h.base, &id, |v| v["state"] == "done", Duration::from_secs(10)).await;
    assert_eq!(done["worker"], "w1");
    assert_eq!(done["exit_code"], 0);
    assert_eq!(done["attempts"], 1);
    let stages: Vec<&str> = done["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["stage"].as_str().unwrap())
        .collect();
    assert_eq!(stages, ["provision", "build", "teardown"]);
    for st in done["stages"].as_array().unwrap() {
        assert!(st["finished_at"].is_string(), "stage not finished: {st}");
        assert!(st["started_at"].as_str() <= st["finished_at"].as_str());
    }

    let log = get(&h.base, &format!("/api/jobs/{id}/log")).await;
    let lines: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["line"].as_str().unwrap())
        .collect();
    assert!(lines[0].starts_with("noop job"));
    assert_eq!(lines.iter().filter(|l| l.starts_with("step ")).count(), 4);

    let failed = post(
        &h.base,
        "/api/jobs",
        json!({"job": {"kind": "noop", "duration_ms": 10, "fail": true}}),
    )
    .await;
    let fid = failed["id"].as_str().unwrap().to_string();
    let f = wait_for(&h.base, &fid, |v| v["state"] == "failed", Duration::from_secs(10)).await;
    assert_eq!(f["exit_code"], 1);
    assert_eq!(f["error"], "noop failure requested");

    worker.abort();
}

#[tokio::test]
async fn job_survives_worker_reconnect() {
    let h = start_coordinator().await;
    let state = WorkerState::new(2);
    let first = tokio::spawn(repoman_worker::run_with_state(
        h.worker_cfg.clone(),
        state.clone(),
    ));
    wait_connected(&h.base, "w1").await;

    let created = post(
        &h.base,
        "/api/jobs",
        json!({"job": {"kind": "noop", "duration_ms": 1500}}),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    wait_for(&h.base, &id, |v| v["state"] == "running", Duration::from_secs(5)).await;

    // Drop the connection mid-job. The executing task is detached from the
    // session, so it keeps running and buffering reports.
    first.abort();
    wait_for(&h.base, &id, |v| v["state"] == "orphaned", Duration::from_secs(5)).await;
    assert_eq!(state.in_flight(), vec![id.clone()]);

    let second = tokio::spawn(repoman_worker::run_with_state(h.worker_cfg.clone(), state.clone()));
    let done = wait_for(&h.base, &id, |v| v["state"] == "done", Duration::from_secs(10)).await;
    assert_eq!(done["attempts"], 1, "resumed, not re-run");
    assert_eq!(done["worker"], "w1");
    let stages = done["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 3);
    assert!(stages.iter().all(|s| s["finished_at"].is_string()));

    let log = get(&h.base, &format!("/api/jobs/{id}/log")).await;
    let steps: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l["line"].as_str())
        .filter(|l| l.starts_with("step "))
        .collect();
    assert_eq!(steps, ["step 1/4", "step 2/4", "step 3/4", "step 4/4"], "log order preserved across reconnect");

    assert!(state.in_flight().is_empty());
    second.abort();
}

#[tokio::test]
async fn bad_token_is_rejected() {
    let h = start_coordinator().await;
    let mut cfg = h.worker_cfg.clone();
    cfg.token = "wrong".into();
    let res = tokio::time::timeout(Duration::from_secs(5), repoman_worker::run(cfg))
        .await
        .expect("worker should give up promptly on rejection");
    let err = res.unwrap_err().to_string();
    assert!(err.contains("rejected"), "{err}");
    let _ = &h.ws;
}
