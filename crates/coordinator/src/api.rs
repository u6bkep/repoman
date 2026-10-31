//! HTTP API. JSON only in M0; the server-rendered UI arrives with M2.

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use repoman_common::proto::{JobKind, LogLine};
use repoman_store::{JobRow, StageRow, WorkerRow};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::hub::ConnectedWorker;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/workers", get(list_workers))
        .route("/api/jobs", get(list_jobs).post(create_job))
        .route("/api/jobs/{id}", get(get_job))
        .route("/api/jobs/{id}/log", get(get_job_log))
        .route("/ws/worker", get(worker_ws))
        .with_state(state)
}

type ApiError = (StatusCode, String);

fn internal(e: anyhow::Error) -> ApiError {
    tracing::error!(error = %e, "api error");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[derive(Serialize)]
struct Health {
    ok: bool,
    version: &'static str,
}

async fn health() -> impl IntoResponse {
    axum::Json(Health {
        ok: true,
        version: repoman_common::VERSION,
    })
}

#[derive(Serialize)]
pub struct WorkerView {
    #[serde(flatten)]
    pub row: WorkerRow,
    pub connected: bool,
    pub running: Vec<String>,
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

async fn list_jobs(
    State(st): State<AppState>,
    Query(q): Query<ListJobsQuery>,
) -> Result<axum::Json<Vec<JobRow>>, ApiError> {
    Ok(axum::Json(
        st.store.list_jobs(q.limit.min(1000)).map_err(internal)?,
    ))
}

#[derive(Deserialize)]
pub struct CreateJob {
    pub job: JobKind,
    #[serde(default)]
    pub pkgbase: Option<String>,
    #[serde(default)]
    pub stream: Option<String>,
}

async fn create_job(
    State(st): State<AppState>,
    axum::Json(req): axum::Json<CreateJob>,
) -> Result<(StatusCode, axum::Json<JobRow>), ApiError> {
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

async fn get_job(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::Json<JobView>, ApiError> {
    let row = st
        .store
        .get_job(&id)
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such job".to_string()))?;
    let stages = st.store.job_stages(&id).map_err(internal)?;
    Ok(axum::Json(JobView { row, stages }))
}

async fn get_job_log(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::Json<Vec<LogLine>>, ApiError> {
    if st.store.get_job(&id).map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such job".to_string()));
    }
    Ok(axum::Json(st.store.job_log(&id).map_err(internal)?))
}

async fn worker_ws(ws: WebSocketUpgrade, State(st): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| st.hub.handle_socket(socket))
}
