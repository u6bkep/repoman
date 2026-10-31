//! HTTP API. JSON for the CLI and future UI, plus file serving for workers
//! and pacman clients: the blob store, staging repos, published repos, and
//! source tarballs.

use axum::Router;
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use repoman_common::proto::{JobKind, LogLine};
use repoman_store::{BaselineRow, CandidateRow, JobRow, MembershipRow, PackageRow, SnapshotRow, StageRow, WorkerRow};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;

use crate::AppState;
use crate::hub::ConnectedWorker;

pub fn router(state: AppState) -> Router {
    let bulk = state.config.bulk_dir();
    Router::new()
        .route("/api/health", get(health))
        .route("/api/workers", get(list_workers))
        .route("/api/jobs", get(list_jobs).post(create_job))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/jobs/{id}/log", get(get_job_log))
        .route("/api/blobs/{sha256}", axum::routing::put(put_blob))
        .route("/api/sources/{pkgbase}/{file}", get(get_source))
        .route("/api/packages", get(list_packages).post(add_package))
        .route("/api/packages/{pkgbase}", get(get_package).delete(remove_package))
        .route("/api/packages/{pkgbase}/policy", post(set_policy))
        .route("/api/packages/{pkgbase}/streams/{stream}", axum::routing::delete(remove_membership))
        .route("/api/runs", get(list_runs).post(start_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/candidates", get(list_candidates))
        .route("/api/candidates/{id}", get(get_candidate))
        .route("/api/candidates/{id}/decide", post(decide_candidate))
        .route("/api/images", get(list_images))
        .route("/api/signing-key", post(generate_signing_key))
        .route("/repos/repoman.asc", get(public_key))
        .route("/ws/worker", get(worker_ws))
        .nest_service("/store", ServeDir::new(bulk.join("store")))
        .nest_service("/staging", ServeDir::new(bulk.join("staging")))
        .nest_service("/repos", ServeDir::new(bulk.join("repos")))
        .with_state(state)
}

type ApiError = (StatusCode, String);

fn internal(e: anyhow::Error) -> ApiError {
    tracing::error!(error = %format!("{e:#}"), "api error");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn not_found(msg: impl Into<String>) -> ApiError {
    (StatusCode::NOT_FOUND, msg.into())
}

#[derive(Serialize)]
struct Health {
    ok: bool,
    version: &'static str,
    run_active: bool,
}

async fn health(State(st): State<AppState>) -> impl IntoResponse {
    axum::Json(Health {
        ok: true,
        version: repoman_common::VERSION,
        run_active: st.engine.is_running(),
    })
}

// ---- workers and jobs -------------------------------------------------------

#[derive(Serialize)]
pub struct WorkerView {
    #[serde(flatten)]
    pub row: WorkerRow,
    pub connected: bool,
    pub running: Vec<String>,
    pub capabilities: Vec<String>,
}

async fn list_workers(State(st): State<AppState>) -> Result<axum::Json<Vec<WorkerView>>, ApiError> {
    let connected: Vec<ConnectedWorker> = st.hub.connected_workers();
    let rows = st.store.list_workers().map_err(internal)?;
    let views = rows
        .into_iter()
        .map(|row| {
            let live = connected.iter().find(|c| c.name == row.name);
            WorkerView {
                connected: live.is_some(),
                running: live.map(|c| c.running.clone()).unwrap_or_default(),
                capabilities: live.map(|c| c.capabilities.clone()).unwrap_or_default(),
                row,
            }
        })
        .collect();
    Ok(axum::Json(views))
}

#[derive(Deserialize)]
struct ListJobsQuery {
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    50
}

async fn list_jobs(State(st): State<AppState>, Query(q): Query<ListJobsQuery>) -> Result<axum::Json<Vec<JobRow>>, ApiError> {
    Ok(axum::Json(st.store.list_jobs(q.limit.min(1000)).map_err(internal)?))
}

#[derive(Deserialize)]
pub struct CreateJob {
    pub job: JobKind,
    #[serde(default)]
    pub pkgbase: Option<String>,
    #[serde(default)]
    pub stream: Option<String>,
}

async fn create_job(State(st): State<AppState>, axum::Json(req): axum::Json<CreateJob>) -> Result<(StatusCode, axum::Json<JobRow>), ApiError> {
    if !matches!(req.job, JobKind::Noop { .. } | JobKind::Probe { .. }) {
        return Err(bad_request("only noop and probe jobs can be submitted directly; builds come from runs"));
    }
    let row = st
        .store
        .create_job(&req.job, req.pkgbase.as_deref(), req.stream.as_deref())
        .map_err(internal)?;
    st.store
        .event("job.created", Some(&row.id), serde_json::json!({"kind": row.job.name()}))
        .map_err(internal)?;
    st.hub.notify();
    Ok((StatusCode::CREATED, axum::Json(row)))
}

#[derive(Serialize)]
pub struct JobView {
    #[serde(flatten)]
    pub row: JobRow,
    pub stages: Vec<StageRow>,
}

async fn get_job(State(st): State<AppState>, Path(id): Path<String>) -> Result<axum::Json<JobView>, ApiError> {
    let row = st.store.get_job(&id).map_err(internal)?.ok_or(not_found("no such job"))?;
    let stages = st.store.job_stages(&id).map_err(internal)?;
    Ok(axum::Json(JobView { row, stages }))
}

async fn get_job_log(State(st): State<AppState>, Path(id): Path<String>) -> Result<axum::Json<Vec<LogLine>>, ApiError> {
    if st.store.get_job(&id).map_err(internal)?.is_none() {
        return Err(not_found("no such job"));
    }
    Ok(axum::Json(st.store.job_log(&id).map_err(internal)?))
}

async fn worker_ws(ws: WebSocketUpgrade, State(st): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| st.hub.handle_socket(socket))
}

// ---- blobs and sources ------------------------------------------------------

fn check_worker_token(st: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "missing bearer token".to_string()))?;
    if st.config.workers.iter().any(|w| w.token == auth) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, "bad token".to_string()))
    }
}

#[derive(Deserialize)]
struct BlobQuery {
    name: String,
}

async fn put_blob(
    State(st): State<AppState>,
    Path(sha256): Path<String>,
    Query(q): Query<BlobQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, ApiError> {
    check_worker_token(&st, &headers)?;
    let stream = body.into_data_stream();
    st.blobs
        .put(&sha256, &q.name, stream)
        .await
        .map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(StatusCode::CREATED)
}

/// `GET /api/sources/{pkgbase}/{commit}.tar`
async fn get_source(State(st): State<AppState>, Path((pkgbase, file)): Path<(String, String)>) -> Result<Response, ApiError> {
    let commit = file.strip_suffix(".tar").ok_or(bad_request("expected <commit>.tar"))?;
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad_request("bad commit"));
    }
    if !st.aur.has_commit(&pkgbase, commit).await {
        return Err(not_found("unknown package or commit"));
    }
    let bytes = st.aur.archive(&pkgbase, commit).await.map_err(internal)?;
    Ok((
        [(header::CONTENT_TYPE, "application/x-tar")],
        bytes,
    )
        .into_response())
}

// ---- packages ---------------------------------------------------------------

#[derive(Serialize)]
pub struct PackageView {
    #[serde(flatten)]
    pub row: PackageRow,
    pub streams: Vec<MembershipRow>,
    pub baseline: Option<BaselineRow>,
    pub snapshot: Option<SnapshotRow>,
    pub open_candidates: Vec<CandidateRow>,
}

fn package_view(st: &AppState, row: PackageRow) -> Result<PackageView, ApiError> {
    Ok(PackageView {
        streams: st.store.memberships(&row.pkgbase).map_err(internal)?,
        baseline: st.store.get_baseline(&row.pkgbase).map_err(internal)?,
        snapshot: st.store.get_snapshot(&row.pkgbase).map_err(internal)?,
        open_candidates: st.store.open_candidates(&row.pkgbase).map_err(internal)?,
        row,
    })
}

async fn list_packages(State(st): State<AppState>) -> Result<axum::Json<Vec<PackageView>>, ApiError> {
    let rows = st.store.list_packages().map_err(internal)?;
    rows.into_iter().map(|r| package_view(&st, r)).collect::<Result<Vec<_>, _>>().map(axum::Json)
}

#[derive(Deserialize)]
pub struct AddPackage {
    pub pkgbase: String,
    /// Streams to want it on. Empty means every configured stream.
    #[serde(default)]
    pub streams: Vec<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// Git URL of a repository with the PKGBUILD at its root; default AUR.
    #[serde(default)]
    pub source: Option<String>,
}

async fn add_package(State(st): State<AppState>, axum::Json(req): axum::Json<AddPackage>) -> Result<(StatusCode, axum::Json<PackageView>), ApiError> {
    let pkgbase = req.pkgbase.trim();
    if pkgbase.is_empty() {
        return Err(bad_request("pkgbase required"));
    }
    let streams: Vec<String> = if req.streams.is_empty() {
        st.config.streams.iter().map(|s| s.name.clone()).collect()
    } else {
        for s in &req.streams {
            if st.config.stream(s).is_none() {
                return Err(bad_request(format!("unknown stream {s}")));
            }
        }
        req.streams.clone()
    };
    let kind = req
        .kind
        .clone()
        .unwrap_or_else(|| if pkgbase.ends_with("-git") { "vcs".into() } else { "release".into() });
    if let Some(src) = &req.source {
        if !(src.starts_with("https://") || src.starts_with("http://") || src.starts_with("file://") || src.starts_with("git://")) {
            return Err(bad_request("source must be an http(s), git, or file URL"));
        }
    }
    let created = st
        .store
        .add_package_from(pkgbase, &kind, req.class.as_deref(), req.source.as_deref())
        .map_err(internal)?;
    if !created {
        if let Some(c) = &req.class {
            st.store.set_package_class(pkgbase, c).map_err(internal)?;
        }
        if req.source.is_some() {
            st.store.set_package_source(pkgbase, req.source.as_deref()).map_err(internal)?;
        }
    }
    for s in &streams {
        let existing = st.store.memberships(pkgbase).map_err(internal)?;
        let required = existing.iter().find(|m| &m.stream == s).is_some_and(|m| m.required);
        st.store.set_membership(pkgbase, s, true, required).map_err(internal)?;
    }
    st.store
        .event("package.added", Some(pkgbase), serde_json::json!({"streams": streams, "created": created}))
        .map_err(internal)?;
    let row = st.store.get_package(pkgbase).map_err(internal)?.ok_or(not_found("package vanished"))?;
    Ok((if created { StatusCode::CREATED } else { StatusCode::OK }, axum::Json(package_view(&st, row)?)))
}

async fn get_package(State(st): State<AppState>, Path(pkgbase): Path<String>) -> Result<axum::Json<PackageView>, ApiError> {
    let row = st.store.get_package(&pkgbase).map_err(internal)?.ok_or(not_found("no such package"))?;
    Ok(axum::Json(package_view(&st, row)?))
}

async fn remove_package(State(st): State<AppState>, Path(pkgbase): Path<String>) -> Result<StatusCode, ApiError> {
    if !st.store.remove_package(&pkgbase).map_err(internal)? {
        return Err(not_found("no such package"));
    }
    st.store.event("package.removed", Some(&pkgbase), serde_json::json!({})).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_membership(State(st): State<AppState>, Path((pkgbase, stream)): Path<(String, String)>) -> Result<StatusCode, ApiError> {
    if st.store.get_package(&pkgbase).map_err(internal)?.is_none() {
        return Err(not_found("no such package"));
    }
    st.store.remove_membership(&pkgbase, &stream).map_err(internal)?;
    st.store
        .event("package.membership_removed", Some(&pkgbase), serde_json::json!({"stream": stream}))
        .map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SetPolicy {
    pub policy: serde_json::Value,
}

async fn set_policy(State(st): State<AppState>, Path(pkgbase): Path<String>, axum::Json(req): axum::Json<SetPolicy>) -> Result<StatusCode, ApiError> {
    if !req.policy.is_object() {
        return Err(bad_request("policy must be an object"));
    }
    if !st.store.set_package_policy(&pkgbase, &req.policy).map_err(internal)? {
        return Err(not_found("no such package"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- runs and candidates ----------------------------------------------------

async fn list_runs(State(st): State<AppState>) -> Result<axum::Json<Vec<repoman_store::RunRow>>, ApiError> {
    Ok(axum::Json(st.store.list_runs(50).map_err(internal)?))
}

#[derive(Serialize)]
struct StartedRun {
    id: String,
}

async fn start_run(State(st): State<AppState>) -> Result<(StatusCode, axum::Json<StartedRun>), ApiError> {
    if st.config.streams.is_empty() {
        return Err(bad_request("no streams configured"));
    }
    let id = st.engine.start_or_resume(st.clone()).map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok((StatusCode::ACCEPTED, axum::Json(StartedRun { id })))
}

#[derive(Serialize)]
pub struct RunView {
    #[serde(flatten)]
    pub row: repoman_store::RunRow,
    pub candidates: Vec<CandidateRow>,
    pub jobs: Vec<JobRow>,
}

async fn get_run(State(st): State<AppState>, Path(id): Path<String>) -> Result<axum::Json<RunView>, ApiError> {
    let row = st.store.get_run(&id).map_err(internal)?.ok_or(not_found("no such run"))?;
    let candidates = st.store.candidates_for_run(&id).map_err(internal)?;
    let jobs = st
        .store
        .list_jobs(1000)
        .map_err(internal)?
        .into_iter()
        .filter(|j| candidates.iter().any(|c| Some(&c.id) == j.candidate_id.as_ref()))
        .collect();
    Ok(axum::Json(RunView { row, candidates, jobs }))
}

#[derive(Deserialize)]
struct CandidatesQuery {
    #[serde(default)]
    state: Option<String>,
}

async fn list_candidates(State(st): State<AppState>, Query(q): Query<CandidatesQuery>) -> Result<axum::Json<Vec<CandidateRow>>, ApiError> {
    let states: Vec<&str> = match q.state.as_deref() {
        Some("all") => repoman_store::runs::OPEN_STATES.iter().copied().chain(["published", "rejected", "superseded"]).collect(),
        Some(s) => vec![s],
        None => vec!["pending"],
    };
    Ok(axum::Json(st.store.candidates_in_states(&states).map_err(internal)?))
}

async fn get_candidate(State(st): State<AppState>, Path(id): Path<String>) -> Result<axum::Json<CandidateRow>, ApiError> {
    Ok(axum::Json(st.store.get_candidate(&id).map_err(internal)?.ok_or(not_found("no such candidate"))?))
}

#[derive(Deserialize)]
pub struct Decision {
    /// approve | reject | defer
    pub decision: String,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

async fn decide_candidate(State(st): State<AppState>, Path(id): Path<String>, axum::Json(d): axum::Json<Decision>) -> Result<axum::Json<CandidateRow>, ApiError> {
    let c = st.store.get_candidate(&id).map_err(internal)?.ok_or(not_found("no such candidate"))?;
    if !matches!(c.state.as_str(), "pending" | "deferred" | "failed") {
        return Err((StatusCode::CONFLICT, format!("candidate is {}", c.state)));
    }
    let state = match d.decision.as_str() {
        "approve" => "approved",
        "reject" => "rejected",
        "defer" => "deferred",
        other => return Err(bad_request(format!("unknown decision {other}"))),
    };
    if state == "approved" && c.kind == "onboarding" {
        let class = d
            .class
            .clone()
            .or_else(|| st.store.get_package(&c.pkgbase).ok().flatten().and_then(|p| p.provenance_class));
        let Some(class) = class else {
            return Err(bad_request("onboarding approval needs a provenance class"));
        };
        st.store.set_package_class(&c.pkgbase, &class).map_err(internal)?;
    } else if let Some(class) = &d.class {
        st.store.set_package_class(&c.pkgbase, class).map_err(internal)?;
    }
    st.store
        .decide_candidate(&id, state, d.by.as_deref().unwrap_or("cli"), d.note.as_deref())
        .map_err(internal)?;
    st.store
        .event("candidate.decided", Some(&id), serde_json::json!({"pkgbase": c.pkgbase, "state": state}))
        .map_err(internal)?;
    Ok(axum::Json(st.store.get_candidate(&id).map_err(internal)?.unwrap()))
}

// ---- images and keys --------------------------------------------------------

async fn list_images(State(st): State<AppState>) -> Result<axum::Json<Vec<repoman_store::ImageRow>>, ApiError> {
    Ok(axum::Json(st.store.list_images().map_err(internal)?))
}

#[derive(Deserialize)]
pub struct KeyGen {
    #[serde(default = "default_uid")]
    pub uid: String,
}

fn default_uid() -> String {
    "repoman <repoman@localhost>".to_string()
}

#[derive(Serialize)]
struct KeyGenResult {
    fingerprint: String,
    note: &'static str,
}

async fn generate_signing_key(State(st): State<AppState>, axum::Json(req): axum::Json<KeyGen>) -> Result<axum::Json<KeyGenResult>, ApiError> {
    if st.publisher.signing_enabled() {
        return Err((StatusCode::CONFLICT, "a signing key is already configured".into()));
    }
    let fingerprint = st.publisher.generate_key(&req.uid).await.map_err(internal)?;
    Ok(axum::Json(KeyGenResult {
        fingerprint,
        note: "set signing_key in coordinator.toml to this fingerprint and restart",
    }))
}

async fn public_key(State(st): State<AppState>) -> Result<Response, ApiError> {
    let key = st.publisher.export_public_key().await.map_err(|e| not_found(format!("{e:#}")))?;
    Ok(([(header::CONTENT_TYPE, "application/pgp-keys")], key).into_response())
}
