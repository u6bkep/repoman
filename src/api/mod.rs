use axum::{
    Json, Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket},
    },
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::aur::{AurClient, AurGit, DepResolver, read_srcinfo};
use crate::build::{BuildSystem, vercmp};
use crate::models::{BuildStage, InstallReason, Package};
use crate::service::status::{DerivedStatus, StatusInputs, derive_status};
pub use crate::state::{AppState, SharedState, SrcInfoCache};

/// Server-sent event for real-time status updates
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    PackageStatusChanged {
        package: PackageResponse,
    },
    BuildStarted {
        build_id: String,
        pkgbase: String,
    },
    BuildStageChanged {
        build_id: String,
        pkgbase: String,
        stage: String,
    },
    BuildCompleted {
        build_id: String,
        pkgbase: String,
        status: String,
    },
    CheckUpdatesProgress {
        checked: usize,
        total: usize,
        current: String,
    },
    CheckUpdatesComplete {
        checked: usize,
        updated: Vec<String>,
    },
}

/// Create the API router
pub fn router(state: SharedState) -> Router {
    Router::new()
        // Package endpoints
        .route("/api/packages", get(list_packages).post(add_package))
        .route(
            "/api/packages/{pkgbase}",
            get(get_package)
                .delete(remove_package)
                .patch(update_package),
        )
        .route("/api/packages/{pkgbase}/pkgbuild", get(get_pkgbuild))
        .route("/api/packages/{pkgbase}/diff", get(get_diff))
        .route("/api/packages/{pkgbase}/approve", post(approve_package))
        // reject endpoint removed in V2
        .route("/api/packages/{pkgbase}/build", post(trigger_build))
        .route("/api/packages/{pkgbase}/builds", get(list_builds))
        .route("/api/packages/{pkgbase}/mark", post(mark_package))
        .route("/api/packages/{pkgbase}/deps", get(get_package_deps))
        .route(
            "/api/packages/{pkgbase}/check-update",
            post(check_package_update),
        )
        .route("/api/packages/check-updates", post(check_updates))
        .route(
            "/api/packages/approve-and-build",
            post(approve_and_build_updates),
        )
        .route("/api/packages/orphans", get(list_orphans))
        // AUR endpoints
        .route("/api/aur/search", get(search_aur))
        .route("/api/aur/info/{name}", get(aur_info))
        // Build endpoints
        .route("/api/builds", get(list_all_builds))
        .route("/api/builds/queue", post(queue_builds))
        .route("/api/builds/{id}", get(get_build))
        .route("/api/builds/{id}/logs", get(get_build_logs))
        .route("/api/builds/{id}/logs/stream", get(stream_build_logs))
        .route("/api/builds/{id}/cancel", post(cancel_build))
        // System endpoints
        .route("/api/repo", get(repo_info))
        .route("/api/repo/clean", post(clean_repo))
        .route("/api/repo/rebuild-db", post(rebuild_repo_db))
        .route("/api/repo/packages/{name}", delete(remove_from_repo))
        .route("/api/system/status", get(system_status))
        .route("/api/system/image/rebuild", post(rebuild_image))
        .route("/api/system/cache/clean", post(clean_cache))
        .route("/api/system/sources/clean", post(clean_sources))
        .route("/api/system/orphans/clean", post(clean_orphans))
        .route("/api/system/containers", get(list_containers))
        .route("/api/system/containers/prune", post(prune_containers))
        .route("/api/system/images", get(list_images))
        .route("/api/system/images/prune", post(prune_images))
        .route("/api/system/config/template", get(config_template))
        // SSE event stream
        .route("/api/events", get(sse_events))
        // Combined check-and-build endpoint
        .route("/api/packages/check-and-build", post(check_and_build))
        .with_state(state)
}

// ==================== SSE handler ====================

async fn sse_events(
    State(state): State<SharedState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = state.events.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(event) => {
            let json = serde_json::to_string(&event).ok()?;
            Some(Ok(SseEvent::default().data(json)))
        }
        Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
            tracing::warn!("SSE client lagged, skipped {} events", n);
            None
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Emit a server event on the broadcast channel. Ignores send errors (no receivers).
pub fn emit_event(tx: &tokio::sync::broadcast::Sender<ServerEvent>, event: ServerEvent) {
    let _ = tx.send(event);
}

/// Emit a PackageStatusChanged event with the full enriched package object.
pub async fn emit_status_changed(state: &AppState, pkgbase: &str) {
    let pkg = match state.db.get_package(pkgbase).await {
        Ok(Some(pkg)) => pkg,
        _ => return, // Package may have been deleted or DB error
    };
    let resp = assemble_package_response(state, &pkg).await;
    emit_event(
        &state.events,
        ServerEvent::PackageStatusChanged { package: resp },
    );
}

// ==================== Request/Response types ====================

#[derive(Deserialize)]
pub struct AddPackageRequest {
    pub name: String,
    #[serde(default)]
    pub as_dependency: bool,
    /// If true, automatically add missing AUR dependencies
    #[serde(default)]
    pub with_deps: bool,
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct MarkRequest {
    pub install_reason: String,
}

#[derive(Deserialize)]
pub struct UpdatePackageRequest {
    pub auto_approve: Option<bool>,
}

#[derive(Deserialize)]
pub struct DeleteParams {
    pub delete_files: Option<bool>,
    pub delete_clone: Option<bool>,
    pub recursive: Option<bool>,
}

#[derive(Serialize)]
pub struct DeletePackageResponse {
    pub removed: Vec<String>,
    pub dependents_warning: Option<String>,
}

#[derive(Deserialize)]
pub struct PaginationQuery {
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

#[derive(Deserialize)]
pub struct BuildsListQuery {
    pub limit: Option<i32>,
    pub offset: Option<i32>,
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct ApproveQuery {
    pub commit: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct PkgNameResponse {
    pub pkgname: String,
    pub pkgdesc: Option<String>,
    pub depends: Vec<String>,
    pub repo_version: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct PackageResponse {
    pub pkgbase: String,
    pub install_reason: String,
    pub auto_approve: bool,
    pub approved_commit: Option<String>,
    pub current_commit: Option<String>,
    pub pkgver: Option<String>,
    pub status: String,
    pub pkgnames: Vec<PkgNameResponse>,
    pub is_orphan: bool,
    pub added_at: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct AddPackageResponse {
    #[serde(flatten)]
    pub package: PackageResponse,
    pub missing_deps: Vec<crate::aur::MissingDep>,
    pub added_deps: Vec<AddedDependency>,
}

#[derive(Serialize)]
pub struct AddedDependency {
    pub pkgbase: String,
    pub version: String,
}

#[derive(Serialize)]
pub struct PackageDetailResponse {
    #[serde(flatten)]
    pub package: PackageResponse,
    pub aur_info: Option<crate::models::AurPackageInfo>,
    pub builds: Vec<BuildDetailResponse>,
    pub srcinfo: Option<crate::models::SrcInfo>,
    pub dependencies: Option<PackageDepsResponse>,
}

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize)]
pub struct PkgbuildResponse {
    pub content: String,
    pub commit: String,
    pub files: Vec<String>,
}

#[derive(Serialize)]
pub struct DiffResponse {
    pub from_commit: Option<String>,
    pub to_commit: Option<String>,
    pub diff: String,
}

#[derive(Serialize)]
pub struct BuildResponse {
    pub id: String,
    pub status: String,
}

#[derive(Serialize, Clone)]
pub struct BuildDetailResponse {
    pub id: String,
    pub pkgbase: String,
    pub commit: String,
    pub status: crate::models::BuildStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// Outputs as a real JSON array (replaces stringified outputs)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Current stage of the build (for running builds)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_stage: Option<String>,
}

#[derive(Serialize)]
pub struct CleanResponse {
    pub deleted_count: u32,
    pub deleted_files: Vec<String>,
}

#[derive(Serialize)]
pub struct RemoveFromRepoResponse {
    pub pkgbase: String,
    pub deleted_files: Vec<String>,
}

#[derive(Deserialize)]
pub struct QueueBuildsRequest {
    pub packages: Vec<String>,
}

#[derive(Serialize)]
pub struct QueueBuildsResponse {
    pub build_order: Vec<String>,
    pub build_ids: Vec<String>,
    pub skipped: Vec<QueueSkipped>,
}

#[derive(Serialize)]
pub struct QueueSkipped {
    pub pkgbase: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct ApproveAndBuildResponse {
    pub approved: Vec<String>,
    pub build_order: Vec<String>,
    pub build_ids: Vec<String>,
    pub skipped: Vec<QueueSkipped>,
}

#[derive(Serialize)]
pub struct CheckAndBuildResponse {
    pub checked: usize,
    pub updated: Vec<String>,
    pub approved: Vec<String>,
    pub build_order: Vec<String>,
    pub build_ids: Vec<String>,
    pub skipped: Vec<QueueSkipped>,
}

#[derive(Serialize)]
pub struct DependencyInfo {
    pub name: String,
    pub version_constraint: Option<String>,
    pub source: String,
    pub in_repo: bool,
}

#[derive(Serialize)]
pub struct PackageDepsResponse {
    pub depends: Vec<DependencyInfo>,
    pub makedepends: Vec<DependencyInfo>,
    pub checkdepends: Vec<DependencyInfo>,
    pub optdepends: Vec<String>,
}

#[derive(Serialize)]
pub struct SystemStatusResponse {
    pub podman: bool,
    pub image: bool,
    pub repo: Option<crate::repo::RepoStats>,
    pub packages: PackageCounts,
    pub pacman_cache: Option<PacmanCacheStats>,
    pub source_cache: Option<CacheStats>,
    pub orphaned_data: Option<OrphanedData>,
    pub config: crate::config::Config,
}

#[derive(Serialize)]
pub struct PacmanCacheStats {
    pub path: String,
    pub package_count: u32,
    pub total_size: u64,
}

#[derive(Serialize)]
pub struct OrphanedData {
    pub clones: Vec<String>,
    pub sources: Vec<String>,
    pub pkgdests: Vec<String>,
    pub logs: Vec<String>,
    pub total_size: u64,
}

#[derive(Serialize)]
pub struct PackageCounts {
    pub total: u32,
    pub pending_review: u32,
    pub approved: u32,
    pub up_to_date: u32,
    pub update_available: u32,
    pub failed: u32,
}

#[derive(Serialize)]
pub struct CacheStats {
    pub path: String,
    pub package_count: u32,
    pub total_size: u64,
}

#[derive(Serialize)]
pub struct CheckUpdatesResponse {
    pub checked: usize,
    pub updated: Vec<String>,
}

// ==================== Error handling ====================

pub struct AppError {
    status: StatusCode,
    pub message: String,
}

impl AppError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::internal(err.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        Self::internal(err.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        Self::internal(err.to_string())
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        Self::internal(err.to_string())
    }
}

// ==================== Helper functions ====================

/// Derive the status for a single package using V2 derived status logic.
async fn derive_package_status(state: &AppState, pkg: &Package) -> DerivedStatus {
    let clone_dir = pkg.clone_path(&state.config.resolved_cache().aur_clones);

    let cache = state.srcinfo_cache.read().await;
    let head_commit = cache.get(&pkg.pkgbase).map(|c| c.commit.clone());
    drop(cache);

    // If no cached commit, try reading from disk
    let head_commit = match head_commit {
        Some(c) => Some(c),
        None => SrcInfoCache::get_head_commit(&clone_dir).ok().flatten(),
    };

    let active = state.active_builds.read().await;
    let is_active = active.contains(&pkg.pkgbase);
    drop(active);

    let repo = state.repo_state.read().await;
    let in_repo = repo.has_pkgbase(&pkg.pkgbase);
    drop(repo);

    let latest_build = if let Some(ref approved) = pkg.approved_commit {
        state
            .db
            .get_latest_build_for_commit(&pkg.pkgbase, approved)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    derive_status(&StatusInputs {
        approved_commit: pkg.approved_commit.as_deref(),
        head_commit: head_commit.as_deref(),
        latest_build: latest_build.as_ref(),
        is_active_build: is_active,
        in_repo,
    })
}

/// Merge global and per-package deps into a single list.
fn merge_deps(global: &[String], per_pkg: &[String]) -> Vec<String> {
    let mut all = global.to_vec();
    all.extend_from_slice(per_pkg);
    all.sort();
    all.dedup();
    all
}

/// Assemble a PackageResponse from a Package + caches. Does NOT set is_orphan.
pub async fn assemble_package_response(state: &AppState, pkg: &Package) -> PackageResponse {
    let cache = state.srcinfo_cache.read().await;
    let repo = state.repo_state.read().await;

    let srcinfo = cache.get(&pkg.pkgbase);
    let head_commit = srcinfo.map(|c| c.commit.clone());
    let pkgver = srcinfo.map(|s| s.srcinfo.full_version());

    let pkgnames = srcinfo
        .map(|s| {
            s.srcinfo
                .packages
                .iter()
                .map(|p| PkgNameResponse {
                    pkgname: p.pkgname.clone(),
                    pkgdesc: p.pkgdesc.clone().or(s.srcinfo.pkgdesc.clone()),
                    depends: merge_deps(&s.srcinfo.depends, &p.depends),
                    repo_version: repo
                        .get_version_for_pkgname(&p.pkgname)
                        .map(|v| v.to_string()),
                })
                .collect()
        })
        .unwrap_or_default();

    drop(cache);
    drop(repo);

    let status = derive_package_status(state, pkg).await;

    PackageResponse {
        pkgbase: pkg.pkgbase.clone(),
        install_reason: pkg.install_reason.to_string(),
        auto_approve: pkg.auto_approve,
        approved_commit: pkg.approved_commit.clone(),
        current_commit: head_commit,
        pkgver,
        status: status.to_string(),
        pkgnames,
        is_orphan: false, // computed by caller for list endpoint
        added_at: pkg.added_at.to_rfc3339(),
        updated_at: pkg.updated_at.to_rfc3339(),
    }
}

/// Build a BuildDetailResponse with current_stage and parsed outputs.
async fn build_detail_response(
    state: &AppState,
    build: crate::models::Build,
) -> BuildDetailResponse {
    let current_stage = if build.status == crate::models::BuildStatus::Running
        || build.status == crate::models::BuildStatus::Pending
    {
        let stages = state.build_stages.read().await;
        stages.get(&build.id).map(|s| s.to_string())
    } else {
        None
    };

    // Parse outputs from JSON string into real array
    let outputs = build
        .outputs
        .as_ref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());

    BuildDetailResponse {
        id: build.id,
        pkgbase: build.pkgbase,
        commit: build.commit,
        status: build.status,
        started_at: build.started_at,
        finished_at: build.finished_at,
        log_path: build.log_path,
        outputs,
        error_message: build.error_message,
        current_stage,
    }
}

fn parse_reason(s: &str) -> Result<InstallReason, AppError> {
    match s {
        "explicit" => Ok(InstallReason::Explicit),
        "dependency" => Ok(InstallReason::Dependency),
        _ => Err(AppError::bad_request(format!(
            "Invalid reason: {}. Valid values: explicit, dependency",
            s
        ))),
    }
}

// ==================== Package handlers ====================

async fn list_packages(
    State(state): State<SharedState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<PackageResponse>>, AppError> {
    let reason_filter = query.reason.as_ref().map(|r| parse_reason(r)).transpose()?;

    let packages = match reason_filter {
        Some(reason) => state.db.list_packages_by_reason(reason).await?,
        None => state.db.list_packages().await?,
    };

    // Compute orphan set
    let orphan_set = {
        let orphans = crate::service::packages::list_orphans(&state).await?;
        orphans
            .iter()
            .map(|p| p.pkgbase.clone())
            .collect::<HashSet<String>>()
    };

    let mut responses = Vec::new();
    for pkg in &packages {
        let mut resp = assemble_package_response(&state, pkg).await;
        resp.is_orphan = orphan_set.contains(&pkg.pkgbase);
        responses.push(resp);
    }

    // Filter by status if requested (derived status, so filter in memory)
    if let Some(ref status_str) = query.status {
        responses.retain(|r| r.status == *status_str);
    }

    Ok(Json(responses))
}

async fn add_package(
    State(state): State<SharedState>,
    Json(req): Json<AddPackageRequest>,
) -> Result<Json<AddPackageResponse>, AppError> {
    // Check if package already exists
    if let Ok(pkg) = crate::service::packages::resolve_package(&state, &req.name).await {
        let resp = assemble_package_response(&state, &pkg).await;
        return Ok(Json(AddPackageResponse {
            package: resp,
            missing_deps: vec![],
            added_deps: vec![],
        }));
    }

    // Validate base image when with_deps=true
    if req.with_deps {
        match state.build.image_exists() {
            Ok(true) => {}
            Ok(false) => {
                return Err(AppError::bad_request(
                    "Base container image required for dependency detection. Run rebuild-image first.",
                ));
            }
            Err(e) => {
                return Err(AppError::internal(format!(
                    "Failed to check base image: {}. Is Podman installed and running?",
                    e
                )));
            }
        }
    }

    let install_reason = if req.as_dependency {
        InstallReason::Dependency
    } else {
        InstallReason::Explicit
    };

    let result =
        crate::service::packages::add_package_single(&state, &req.name, install_reason).await?;

    let mut added_deps: Vec<AddedDependency> = Vec::new();

    // Find missing AUR dependencies
    let missing_deps =
        crate::service::packages::find_missing_deps(&state, &result.pkg.pkgbase).await?;

    let remaining_missing_deps = if req.with_deps && !missing_deps.is_empty() {
        let mut processed: HashSet<String> = HashSet::new();
        processed.insert(result.pkg.pkgbase.clone());

        let mut to_check: Vec<(String, String)> = missing_deps
            .iter()
            .filter(|d| d.in_aur && d.pkgbase.is_some())
            .map(|d| (d.pkgbase.clone().unwrap(), d.name.clone()))
            .collect();

        while let Some((dep_pkgbase, dep_name)) = to_check.pop() {
            if processed.contains(&dep_pkgbase) {
                continue;
            }
            processed.insert(dep_pkgbase.clone());

            match crate::service::packages::add_package_single(
                &state,
                &dep_name,
                InstallReason::Dependency,
            )
            .await
            {
                Ok(dep_result) => {
                    added_deps.push(AddedDependency {
                        pkgbase: dep_result.pkg.pkgbase.clone(),
                        version: dep_result.version,
                    });

                    if let Ok(dep_missing) =
                        crate::service::packages::find_missing_deps(&state, &dep_result.pkg.pkgbase)
                            .await
                    {
                        for m in dep_missing {
                            if m.in_aur
                                && let Some(pb) = &m.pkgbase
                                && !processed.contains(pb)
                            {
                                to_check.push((pb.clone(), m.name.clone()));
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to add dependency {}: {}", dep_name, e.message);
                }
            }
        }
        vec![]
    } else {
        missing_deps
    };

    let resp = assemble_package_response(&state, &result.pkg).await;
    Ok(Json(AddPackageResponse {
        package: resp,
        missing_deps: remaining_missing_deps,
        added_deps,
    }))
}

async fn get_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PackageDetailResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;
    let aur_info = state.aur.info_single(&pkg.pkgbase).await?;
    let builds_raw = state.db.list_builds(&pkg.pkgbase, 10, 0).await?;

    // Build detail responses with current_stage and parsed outputs
    let mut builds = Vec::new();
    for b in builds_raw {
        builds.push(build_detail_response(&state, b).await);
    }

    // Read .SRCINFO from cache
    let srcinfo = {
        let cache = state.srcinfo_cache.read().await;
        cache.get(&pkg.pkgbase).map(|c| c.srcinfo.clone())
    };

    // Inline deps
    let dependencies = if let Some(ref si) = srcinfo {
        let tracked_pkgnames = crate::service::packages::get_tracked_pkgnames(&state).await?;
        Some(build_deps_response(si, &state.aur, &tracked_pkgnames).await)
    } else {
        None
    };

    let resp = assemble_package_response(&state, &pkg).await;

    Ok(Json(PackageDetailResponse {
        package: resp,
        aur_info,
        builds,
        srcinfo,
        dependencies,
    }))
}

async fn remove_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Query(params): Query<DeleteParams>,
) -> Result<Json<DeletePackageResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;

    // Check if any other tracked packages depend on this package
    let cache = state.srcinfo_cache.read().await;
    let pkgnames_set: HashSet<String> = cache
        .get(&pkg.pkgbase)
        .map(|c| {
            c.srcinfo
                .packages
                .iter()
                .map(|p| p.pkgname.clone())
                .collect()
        })
        .unwrap_or_default();

    let all_packages = state.db.list_packages().await?;
    let mut dependents: Vec<String> = Vec::new();
    for other_pkg in &all_packages {
        if other_pkg.pkgbase == pkg.pkgbase {
            continue;
        }
        if let Some(other_cached) = cache.get(&other_pkg.pkgbase) {
            let all_deps: Vec<&str> = other_cached
                .srcinfo
                .depends
                .iter()
                .chain(other_cached.srcinfo.makedepends.iter())
                .chain(other_cached.srcinfo.checkdepends.iter())
                .chain(
                    other_cached
                        .srcinfo
                        .packages
                        .iter()
                        .flat_map(|p| p.depends.iter()),
                )
                .map(|d| DepResolver::dep_name(d))
                .collect();
            if all_deps.iter().any(|d| pkgnames_set.contains(*d)) {
                dependents.push(other_pkg.pkgbase.clone());
            }
        }
    }
    drop(cache);
    dependents.sort();
    dependents.dedup();

    let dependents_warning = if dependents.is_empty() {
        None
    } else {
        Some(format!(
            "Warning: the following packages depend on {}: {}",
            pkg.pkgbase,
            dependents.join(", ")
        ))
    };

    let delete_files = params.delete_files.unwrap_or(false);
    let delete_clone = params.delete_clone.unwrap_or(false);
    let recursive = params.recursive.unwrap_or(false);

    let mut removed = vec![pkg.pkgbase.clone()];
    do_remove(&state, &pkg.pkgbase, delete_files, delete_clone).await?;

    if recursive {
        loop {
            let orphans = crate::service::packages::list_orphans(&state).await?;
            if orphans.is_empty() {
                break;
            }
            for orphan in orphans {
                removed.push(orphan.pkgbase.clone());
                do_remove(&state, &orphan.pkgbase, delete_files, delete_clone).await?;
            }
        }
    }

    Ok(Json(DeletePackageResponse {
        removed,
        dependents_warning,
    }))
}

/// Remove a single package from DB, optionally from repo and clone.
async fn do_remove(
    state: &AppState,
    pkgbase: &str,
    delete_files: bool,
    delete_clone: bool,
) -> Result<(), AppError> {
    if delete_files {
        let cache = state.srcinfo_cache.read().await;
        let pkgnames: Vec<String> = cache
            .get(pkgbase)
            .map(|c| {
                c.srcinfo
                    .packages
                    .iter()
                    .map(|p| p.pkgname.clone())
                    .collect()
            })
            .unwrap_or_default();
        drop(cache);

        if !pkgnames.is_empty() {
            let pkgname_refs: Vec<&str> = pkgnames.iter().map(|s| s.as_str()).collect();
            let _repo_lock = state.repo_mutex.lock().await;
            let _ = state.build.repo_remove(&pkgname_refs);
            // Reload RepoState so derive_status sees the package is gone
            let mut repo = state.repo_state.write().await;
            let _ = repo.reload();
            drop(repo);
            drop(_repo_lock);
            for name in &pkgnames {
                let _ = state.repo.delete_package_files(name);
            }
        }
    }

    if delete_clone {
        let clone_path = state.config.resolved_cache().aur_clones.join(pkgbase);
        if clone_path.exists() {
            let _ = std::fs::remove_dir_all(&clone_path);
        }
    }

    state.db.delete_package(pkgbase).await?;

    // Remove from SrcInfoCache
    {
        let mut cache = state.srcinfo_cache.write().await;
        cache.remove(pkgbase);
    }

    Ok(())
}

async fn get_pkgbuild(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PkgbuildResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;
    let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    AurGit::ensure_cloned(&pkg.pkgbase, &clone_path)?;
    let pkgbuild_path = clone_path.join("PKGBUILD");

    let content = std::fs::read_to_string(&pkgbuild_path)?;
    let commit = AurGit::head_commit(&clone_path)?;

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&clone_path)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str()
            && (!name.starts_with('.') || name == ".SRCINFO")
        {
            files.push(name.to_string());
        }
    }

    Ok(Json(PkgbuildResponse {
        content,
        commit,
        files,
    }))
}

async fn get_diff(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<DiffResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;
    let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    AurGit::ensure_cloned(&pkg.pkgbase, &clone_path)?;

    // Get head commit from cache or disk
    let head_commit = {
        let cache = state.srcinfo_cache.read().await;
        cache
            .get(&pkg.pkgbase)
            .map(|c| c.commit.clone())
            .or_else(|| SrcInfoCache::get_head_commit(&clone_path).ok().flatten())
    };

    let diff = if let Some(ref approved) = pkg.approved_commit {
        let current = head_commit.as_deref().unwrap_or("HEAD");
        AurGit::diff(&clone_path, approved, current)?
    } else {
        let pkgbuild = std::fs::read_to_string(clone_path.join("PKGBUILD"))?;
        format!(
            "+++ PKGBUILD (new file)\n{}",
            pkgbuild
                .lines()
                .map(|l| format!("+{}", l))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    Ok(Json(DiffResponse {
        from_commit: pkg.approved_commit,
        to_commit: head_commit,
        diff,
    }))
}

async fn approve_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Query(query): Query<ApproveQuery>,
) -> Result<StatusCode, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;

    // Determine which commit to approve
    let commit = if let Some(c) = query.commit {
        c
    } else {
        // Use HEAD commit from cache or disk
        let cache = state.srcinfo_cache.read().await;
        let head = cache.get(&pkg.pkgbase).map(|c| c.commit.clone());
        drop(cache);

        match head {
            Some(c) => c,
            None => {
                let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
                AurGit::head_commit(&clone_path)?
            }
        }
    };

    state
        .db
        .approve_package(&pkg.pkgbase, &commit, false)
        .await?;

    emit_status_changed(&state, &pkg.pkgbase).await;

    Ok(StatusCode::OK)
}

async fn approve_and_build_updates(
    State(state): State<SharedState>,
) -> Result<Json<ApproveAndBuildResponse>, AppError> {
    let mut approved_pkgbases: Vec<String> = Vec::new();
    let mut buildable_pkgbases: Vec<String> = Vec::new();

    let packages = state.db.list_packages().await?;
    for pkg in &packages {
        let status = derive_package_status(&state, pkg).await;
        match status {
            DerivedStatus::UpdateAvailable | DerivedStatus::PendingReview => {
                // Get HEAD commit to approve
                let cache = state.srcinfo_cache.read().await;
                let head = cache.get(&pkg.pkgbase).map(|c| c.commit.clone());
                drop(cache);

                if let Some(commit) = head {
                    state
                        .db
                        .approve_package(&pkg.pkgbase, &commit, false)
                        .await?;
                    emit_status_changed(&state, &pkg.pkgbase).await;
                    approved_pkgbases.push(pkg.pkgbase.clone());
                    buildable_pkgbases.push(pkg.pkgbase.clone());
                }
            }
            DerivedStatus::Approved => {
                buildable_pkgbases.push(pkg.pkgbase.clone());
            }
            _ => {}
        }
    }

    if buildable_pkgbases.is_empty() {
        return Ok(Json(ApproveAndBuildResponse {
            approved: vec![],
            build_order: vec![],
            build_ids: vec![],
            skipped: vec![],
        }));
    }

    let queue_req = QueueBuildsRequest {
        packages: buildable_pkgbases,
    };
    let queue_resp = queue_builds_inner(&state, queue_req).await?;

    Ok(Json(ApproveAndBuildResponse {
        approved: approved_pkgbases,
        build_order: queue_resp.build_order,
        build_ids: queue_resp.build_ids,
        skipped: queue_resp.skipped,
    }))
}

async fn check_and_build(
    State(state): State<SharedState>,
) -> Result<Json<CheckAndBuildResponse>, AppError> {
    let check_result = crate::service::updates::check_updates_inner(&state).await?;

    let mut approved_pkgbases: Vec<String> = Vec::new();

    let packages = state.db.list_packages().await?;
    for pkg in &packages {
        let status = derive_package_status(&state, pkg).await;
        match status {
            DerivedStatus::Approved => {
                // Already approved, just queue for building
                approved_pkgbases.push(pkg.pkgbase.clone());
            }
            DerivedStatus::UpdateAvailable | DerivedStatus::PendingReview => {
                let cache = state.srcinfo_cache.read().await;
                let head = cache.get(&pkg.pkgbase).map(|c| c.commit.clone());
                drop(cache);

                if let Some(commit) = head {
                    state
                        .db
                        .approve_package(&pkg.pkgbase, &commit, false)
                        .await?;
                    emit_status_changed(&state, &pkg.pkgbase).await;
                    approved_pkgbases.push(pkg.pkgbase.clone());
                }
            }
            _ => {}
        }
    }

    if approved_pkgbases.is_empty() {
        return Ok(Json(CheckAndBuildResponse {
            checked: check_result.checked,
            updated: check_result.updated,
            approved: vec![],
            build_order: vec![],
            build_ids: vec![],
            skipped: vec![],
        }));
    }

    let queue_req = QueueBuildsRequest {
        packages: approved_pkgbases.clone(),
    };
    let queue_resp = queue_builds_inner(&state, queue_req).await?;

    Ok(Json(CheckAndBuildResponse {
        checked: check_result.checked,
        updated: check_result.updated,
        approved: approved_pkgbases,
        build_order: queue_resp.build_order,
        build_ids: queue_resp.build_ids,
        skipped: queue_resp.skipped,
    }))
}

async fn trigger_build(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<BuildResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;

    // Check if already building
    {
        let active = state.active_builds.read().await;
        if active.contains(&pkg.pkgbase) {
            return Err(AppError::bad_request("Package is already building"));
        }
    }

    // Check status is appropriate for building — only Approved packages can build.
    // UpdateAvailable means HEAD differs from approved_commit, so approval is needed first.
    let status = derive_package_status(&state, &pkg).await;
    if status != DerivedStatus::Approved {
        return Err(AppError::bad_request(
            "Package must be approved before building",
        ));
    }

    // Check base image exists
    match state.build.image_exists() {
        Ok(true) => {}
        Ok(false) => {
            return Err(AppError::bad_request(
                "Base image not found. Run 'repoman rebuild-image' first.",
            ));
        }
        Err(e) => {
            return Err(AppError::internal(format!(
                "Failed to check base image: {}. Is Podman installed and running?",
                e
            )));
        }
    }

    // Use the approved commit for the build record — we verified status is Approved,
    // so approved_commit must match HEAD.
    let commit = pkg
        .approved_commit
        .clone()
        .ok_or_else(|| AppError::bad_request("Package has no approved commit"))?;

    let build_id = uuid::Uuid::new_v4().to_string();
    let log_filename = format!("{}.log", build_id);
    let log_path = state.config.resolved_cache().logs.join(&log_filename);
    let now = chrono::Utc::now();

    let build = crate::models::Build {
        id: build_id.clone(),
        pkgbase: pkg.pkgbase.clone(),
        commit,
        status: crate::models::BuildStatus::Pending,
        started_at: now,
        finished_at: None,
        log_path: Some(log_filename),
        outputs: None,
        error_message: None,
    };

    state.db.insert_build(&build).await?;

    // Mark as actively building
    {
        let mut active = state.active_builds.write().await;
        active.insert(pkg.pkgbase.clone());
    }

    // Create cancel flag
    {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let mut flags = state.build_cancel_flags.write().await;
        flags.insert(build_id.clone(), cancel_flag);
    }

    emit_event(
        &state.events,
        ServerEvent::BuildStarted {
            build_id: build_id.clone(),
            pkgbase: pkg.pkgbase.clone(),
        },
    );
    emit_status_changed(&state, &pkg.pkgbase).await;

    let config = state.config.clone();
    let state_clone = state.clone();
    let pkgbase_clone = pkg.pkgbase.clone();
    let build_id_clone = build_id.clone();

    tokio::spawn(async move {
        crate::service::builds::run_build_task(
            state_clone,
            pkgbase_clone,
            build_id_clone,
            log_path,
            config,
        )
        .await
    });

    Ok(Json(BuildResponse {
        id: build_id,
        status: "pending".to_string(),
    }))
}

async fn list_builds(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Query(pagination): Query<PaginationQuery>,
) -> Result<Json<Vec<BuildDetailResponse>>, AppError> {
    let limit = pagination.limit.unwrap_or(20).min(100);
    let offset = pagination.offset.unwrap_or(0).max(0);

    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;
    let builds_raw = state.db.list_builds(&pkg.pkgbase, limit, offset).await?;

    let mut builds = Vec::new();
    for b in builds_raw {
        builds.push(build_detail_response(&state, b).await);
    }

    Ok(Json(builds))
}

async fn mark_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Json(req): Json<MarkRequest>,
) -> Result<Json<PackageResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;

    let reason: InstallReason = req
        .install_reason
        .parse()
        .map_err(|e: String| AppError::bad_request(e))?;

    state.db.update_install_reason(&pkg.pkgbase, reason).await?;

    let pkg = state
        .db
        .get_package(&pkg.pkgbase)
        .await?
        .ok_or_else(|| AppError::internal("Package disappeared"))?;

    let resp = assemble_package_response(&state, &pkg).await;
    Ok(Json(resp))
}

async fn update_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Json(req): Json<UpdatePackageRequest>,
) -> Result<Json<PackageResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;

    if let Some(auto_approve) = req.auto_approve {
        state
            .db
            .update_auto_approve(&pkg.pkgbase, auto_approve)
            .await?;
    }

    let pkg = state
        .db
        .get_package(&pkg.pkgbase)
        .await?
        .ok_or_else(|| AppError::internal("Package disappeared"))?;

    let resp = assemble_package_response(&state, &pkg).await;
    Ok(Json(resp))
}

async fn list_orphans(
    State(state): State<SharedState>,
) -> Result<Json<Vec<PackageResponse>>, AppError> {
    let orphans = crate::service::packages::list_orphans(&state).await?;

    let mut responses = Vec::new();
    for pkg in &orphans {
        let mut resp = assemble_package_response(&state, pkg).await;
        resp.is_orphan = true;
        responses.push(resp);
    }

    Ok(Json(responses))
}

async fn get_package_deps(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PackageDepsResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;
    let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    AurGit::ensure_cloned(&pkg.pkgbase, &clone_path)?;

    let srcinfo = {
        let cache = state.srcinfo_cache.read().await;
        cache.get(&pkg.pkgbase).map(|c| c.srcinfo.clone())
    };

    let srcinfo = match srcinfo {
        Some(si) => si,
        None => read_srcinfo(&clone_path)?,
    };

    let tracked_pkgnames = crate::service::packages::get_tracked_pkgnames(&state).await?;
    let resp = build_deps_response(&srcinfo, &state.aur, &tracked_pkgnames).await;

    Ok(Json(resp))
}

/// Build a PackageDepsResponse from srcinfo.
async fn build_deps_response(
    srcinfo: &crate::models::SrcInfo,
    aur: &AurClient,
    tracked_pkgnames: &HashSet<String>,
) -> PackageDepsResponse {
    async fn classify_dep(
        name: &str,
        aur: &AurClient,
        tracked_pkgnames: &HashSet<String>,
    ) -> (String, bool) {
        let in_repo = tracked_pkgnames.contains(name);

        let pacman_check = std::process::Command::new("pacman")
            .args(["-Si", name])
            .output();

        if let Ok(output) = pacman_check
            && output.status.success()
        {
            return ("official".to_string(), in_repo);
        }

        if let Ok(Some(_)) = aur.info_single(name).await {
            if in_repo {
                return ("aur-tracked".to_string(), true);
            }
            return ("aur".to_string(), false);
        }

        let pacman_search = std::process::Command::new("pacman")
            .args(["-Ss", &format!("^{}$", name)])
            .output();

        if let Ok(output) = pacman_search
            && output.status.success()
            && !output.stdout.is_empty()
        {
            return ("official".to_string(), in_repo);
        }

        ("unknown".to_string(), in_repo)
    }

    let mut depends = Vec::new();
    for dep in &srcinfo.depends {
        let (name, constraint) = crate::service::packages::parse_dep(dep);
        let (source, in_repo) = classify_dep(&name, aur, tracked_pkgnames).await;
        depends.push(DependencyInfo {
            name,
            version_constraint: constraint,
            source,
            in_repo,
        });
    }

    let mut makedepends = Vec::new();
    for dep in &srcinfo.makedepends {
        let (name, constraint) = crate::service::packages::parse_dep(dep);
        let (source, in_repo) = classify_dep(&name, aur, tracked_pkgnames).await;
        makedepends.push(DependencyInfo {
            name,
            version_constraint: constraint,
            source,
            in_repo,
        });
    }

    let mut checkdepends = Vec::new();
    for dep in &srcinfo.checkdepends {
        let (name, constraint) = crate::service::packages::parse_dep(dep);
        let (source, in_repo) = classify_dep(&name, aur, tracked_pkgnames).await;
        checkdepends.push(DependencyInfo {
            name,
            version_constraint: constraint,
            source,
            in_repo,
        });
    }

    PackageDepsResponse {
        depends,
        makedepends,
        checkdepends,
        optdepends: srcinfo.optdepends.clone(),
    }
}

async fn check_updates(
    State(state): State<SharedState>,
) -> Result<Json<CheckUpdatesResponse>, AppError> {
    let result = crate::service::updates::check_updates_inner(&state).await?;
    Ok(Json(CheckUpdatesResponse {
        checked: result.checked,
        updated: result.updated,
    }))
}

async fn check_package_update(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PackageResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &pkgbase).await?;

    let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    let pb = pkg.pkgbase.clone();
    let cp = clone_path.clone();
    tokio::task::spawn_blocking(move || AurGit::ensure_cloned(&pb, &cp))
        .await
        .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
        .map_err(|e| AppError::internal(e.to_string()))?;

    let cp = clone_path.clone();
    let has_updates = tokio::task::spawn_blocking(move || AurGit::has_updates(&cp))
        .await
        .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
        .map_err(|e| AppError::internal(e.to_string()))?;

    if has_updates {
        let cp = clone_path.clone();
        tokio::task::spawn_blocking(move || AurGit::reset_to_remote(&cp))
            .await
            .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
            .map_err(|e| AppError::internal(e.to_string()))?;

        // Re-read and update cache
        let cp = clone_path.clone();
        let (new_commit, srcinfo) = tokio::task::spawn_blocking(move || {
            let commit = AurGit::head_commit(&cp)?;
            let srcinfo = read_srcinfo(&cp)?;
            Ok::<_, anyhow::Error>((commit, srcinfo))
        })
        .await
        .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
        .map_err(|e| AppError::internal(e.to_string()))?;

        // Update SrcInfoCache
        {
            let mut cache = state.srcinfo_cache.write().await;
            cache.insert(pkg.pkgbase.clone(), srcinfo, new_commit);
        }
    }

    // Reload package
    let pkg = state
        .db
        .get_package(&pkg.pkgbase)
        .await?
        .ok_or_else(|| AppError::internal("Package disappeared"))?;

    let resp = assemble_package_response(&state, &pkg).await;
    Ok(Json(resp))
}

// ==================== AUR handlers ====================

async fn search_aur(
    State(state): State<SharedState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<crate::models::AurSearchResult>>, AppError> {
    let results = state.aur.search(&query.q).await?;

    let tracked_pkgbases: HashSet<String> = state
        .db
        .list_packages()
        .await?
        .into_iter()
        .map(|p| p.pkgbase)
        .collect();

    let results: Vec<crate::models::AurSearchResult> = results
        .into_iter()
        .map(|info| {
            let tracked = tracked_pkgbases.contains(&info.package_base);
            crate::models::AurSearchResult { info, tracked }
        })
        .collect();

    Ok(Json(results))
}

async fn aur_info(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<Option<crate::models::AurPackageInfo>>, AppError> {
    let info = state.aur.info_single(&name).await?;
    Ok(Json(info))
}

// ==================== Build handlers ====================

async fn list_all_builds(
    State(state): State<SharedState>,
    Query(query): Query<BuildsListQuery>,
) -> Result<Json<Vec<BuildDetailResponse>>, AppError> {
    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0).max(0);

    let builds_raw = state
        .db
        .list_all_builds(limit, offset, query.status.as_deref())
        .await?;

    let mut results = Vec::new();
    for build in builds_raw {
        results.push(build_detail_response(&state, build).await);
    }

    Ok(Json(results))
}

async fn get_build(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<BuildDetailResponse>, AppError> {
    let build = state
        .db
        .get_build(&id)
        .await?
        .ok_or_else(|| AppError::not_found("Build not found"))?;

    let resp = build_detail_response(&state, build).await;
    Ok(Json(resp))
}

async fn get_build_logs(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<String, AppError> {
    let build = state
        .db
        .get_build(&id)
        .await?
        .ok_or_else(|| AppError::not_found("Build not found"))?;

    let logs_dir = state.config.resolved_cache().logs;
    let log_path = build
        .resolve_log_path(&logs_dir)
        .ok_or_else(|| AppError::not_found("No log file for this build"))?;

    let content = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|_| "Log file not found or empty".to_string());
    Ok(content)
}

async fn stream_build_logs(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    let build = state
        .db
        .get_build(&id)
        .await?
        .ok_or_else(|| AppError::not_found("Build not found"))?;

    let logs_dir = state.config.resolved_cache().logs;
    let log_path = build
        .resolve_log_path(&logs_dir)
        .ok_or_else(|| AppError::not_found("No log file for this build"))?;

    let build_id = build.id;
    let pkgbase = build.pkgbase;

    Ok(ws.on_upgrade(move |socket| handle_log_stream(socket, state, build_id, pkgbase, log_path)))
}

#[derive(Serialize)]
struct StageChangeMessage {
    #[serde(rename = "type")]
    msg_type: &'static str,
    stage: String,
}

async fn send_stage_change(socket: &mut WebSocket, stage: BuildStage) -> bool {
    let msg = StageChangeMessage {
        msg_type: "stage",
        stage: stage.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&msg) {
        socket.send(Message::Text(json.into())).await.is_ok()
    } else {
        true
    }
}

async fn handle_log_stream(
    mut socket: WebSocket,
    state: SharedState,
    build_id: String,
    pkgbase: String,
    log_path: std::path::PathBuf,
) {
    tracing::info!("WebSocket connected for build {} logs", build_id);

    let mut last_sent_stage: Option<BuildStage> = None;

    // Send the current stage if build is already in progress
    {
        let stages = state.build_stages.read().await;
        if let Some(&stage) = stages.get(&build_id)
            && send_stage_change(&mut socket, stage).await
        {
            last_sent_stage = Some(stage);
        }
    }

    // Wait for log file to exist
    let mut attempts = 0;
    while !log_path.exists() && attempts < 30 {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        attempts += 1;
    }

    let file = match tokio::fs::File::open(&log_path).await {
        Ok(f) => f,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    format!("[Error opening log file: {}]", e).into(),
                ))
                .await;
            let _ = socket
                .send(Message::Close(Some(CloseFrame {
                    code: 1000,
                    reason: "Log file not available".into(),
                })))
                .await;
            return;
        }
    };

    let mut reader = tokio::io::BufReader::new(file);
    let mut line_buf = String::new();
    let mut last_position: u64 = 0;

    loop {
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(new_stage) = BuildStage::from_log_line(&line_buf) {
                        crate::service::builds::set_build_stage(
                            &state, &build_id, &pkgbase, new_stage,
                        )
                        .await;

                        if last_sent_stage != Some(new_stage)
                            && send_stage_change(&mut socket, new_stage).await
                        {
                            last_sent_stage = Some(new_stage);
                        }
                    }

                    if socket
                        .send(Message::Text(line_buf.clone().into()))
                        .await
                        .is_err()
                    {
                        tracing::info!("WebSocket client disconnected for build {}", build_id);
                        return;
                    }
                }
                Err(e) => {
                    let _ = socket
                        .send(Message::Text(format!("[Error reading log: {}]", e).into()))
                        .await;
                    break;
                }
            }
        }

        if let Ok(pos) = reader.stream_position().await {
            last_position = pos;
        }

        let (build_status, current_stage) = {
            let status = state
                .db
                .get_build(&build_id)
                .await
                .ok()
                .flatten()
                .map(|b| b.status);
            let stages = state.build_stages.read().await;
            let stage = stages.get(&build_id).copied();
            (status, stage)
        };

        if let Some(stage) = current_stage
            && last_sent_stage != Some(stage)
            && send_stage_change(&mut socket, stage).await
        {
            last_sent_stage = Some(stage);
        }

        match build_status {
            Some(crate::models::BuildStatus::Running)
            | Some(crate::models::BuildStatus::Pending) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

                if let Ok(file) = tokio::fs::File::open(&log_path).await {
                    reader = tokio::io::BufReader::new(file);
                    if reader
                        .seek(std::io::SeekFrom::Start(last_position))
                        .await
                        .is_err()
                    {
                        continue;
                    }
                }
            }
            Some(status) => {
                // Build completed - read remaining
                loop {
                    line_buf.clear();
                    match reader.read_line(&mut line_buf).await {
                        Ok(0) => break,
                        Ok(_) => {
                            if let Some(new_stage) = BuildStage::from_log_line(&line_buf)
                                && last_sent_stage != Some(new_stage)
                                && send_stage_change(&mut socket, new_stage).await
                            {
                                last_sent_stage = Some(new_stage);
                            }
                            let _ = socket.send(Message::Text(line_buf.clone().into())).await;
                        }
                        Err(_) => break,
                    }
                }

                let final_stage = match status {
                    crate::models::BuildStatus::Success => BuildStage::Complete,
                    _ => BuildStage::Failed,
                };
                if last_sent_stage != Some(final_stage) {
                    let _ = send_stage_change(&mut socket, final_stage).await;
                }

                let status_msg = match status {
                    crate::models::BuildStatus::Success => "[Build completed successfully]",
                    crate::models::BuildStatus::Failed => "[Build failed]",
                    crate::models::BuildStatus::Cancelled => "[Build cancelled]",
                    _ => "[Build finished]",
                };
                let _ = socket
                    .send(Message::Text(status_msg.to_string().into()))
                    .await;
                break;
            }
            None => {
                let _ = socket
                    .send(Message::Text("[Build not found]".to_string().into()))
                    .await;
                break;
            }
        }

        match tokio::time::timeout(tokio::time::Duration::from_millis(10), socket.recv()).await {
            Ok(Some(Ok(Message::Close(_)))) => {
                tracing::info!("WebSocket close requested for build {}", build_id);
                break;
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                let _ = socket.send(Message::Pong(data)).await;
            }
            Ok(Some(Err(_))) | Ok(None) => {
                break;
            }
            _ => {}
        }
    }

    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: "Stream complete".into(),
        })))
        .await;
    tracing::info!("WebSocket closed for build {}", build_id);
}

async fn queue_builds(
    State(state): State<SharedState>,
    Json(req): Json<QueueBuildsRequest>,
) -> Result<Json<QueueBuildsResponse>, AppError> {
    queue_builds_inner(&state, req).await.map(Json)
}

async fn queue_builds_inner(
    state: &SharedState,
    req: QueueBuildsRequest,
) -> Result<QueueBuildsResponse, AppError> {
    if req.packages.is_empty() {
        return Err(AppError::bad_request("No packages specified"));
    }

    match state.build.image_exists() {
        Ok(true) => {}
        Ok(false) => {
            return Err(AppError::bad_request(
                "Base image not found. Run 'repoman rebuild-image' first.",
            ));
        }
        Err(e) => {
            return Err(AppError::internal(format!(
                "Failed to check base image: {}. Is Podman installed and running?",
                e
            )));
        }
    }

    // Resolve all packages and check they're ready to build
    let mut packages_to_build: Vec<Package> = Vec::new();
    let mut skipped: Vec<QueueSkipped> = Vec::new();

    for name in &req.packages {
        match crate::service::packages::resolve_package(state, name).await {
            Ok(pkg) => {
                let status = derive_package_status(state, &pkg).await;
                if status != DerivedStatus::Approved && status != DerivedStatus::UpdateAvailable {
                    skipped.push(QueueSkipped {
                        pkgbase: pkg.pkgbase,
                        reason: format!(
                            "Package status is '{}', must be 'approved' or 'update_available'",
                            status
                        ),
                    });
                } else {
                    // Verify we can get a commit
                    let cache = state.srcinfo_cache.read().await;
                    let has_commit = cache.get(&pkg.pkgbase).is_some();
                    drop(cache);

                    if !has_commit {
                        let clone_dir = pkg.clone_path(&state.config.resolved_cache().aur_clones);
                        if SrcInfoCache::get_head_commit(&clone_dir)
                            .ok()
                            .flatten()
                            .is_none()
                        {
                            skipped.push(QueueSkipped {
                                pkgbase: pkg.pkgbase,
                                reason: "Package has no current commit".to_string(),
                            });
                            continue;
                        }
                    }
                    packages_to_build.push(pkg);
                }
            }
            Err(_) => {
                skipped.push(QueueSkipped {
                    pkgbase: name.clone(),
                    reason: "Package not found".to_string(),
                });
            }
        }
    }

    if packages_to_build.is_empty() {
        return Ok(QueueBuildsResponse {
            build_order: vec![],
            build_ids: vec![],
            skipped,
        });
    }

    // Build dependency graph using srcinfo cache
    let pkgbases_set: HashSet<String> = packages_to_build
        .iter()
        .map(|p| p.pkgbase.clone())
        .collect();

    let cache = state.srcinfo_cache.read().await;

    // Map pkgnames to pkgbases for dependency lookup
    let mut pkgname_to_pkgbase: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for pkg in &packages_to_build {
        if let Some(cached) = cache.get(&pkg.pkgbase) {
            for sp in &cached.srcinfo.packages {
                pkgname_to_pkgbase.insert(sp.pkgname.clone(), pkg.pkgbase.clone());
            }
        }
    }

    // Build the dependency graph
    let mut deps_graph: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for pkg in &packages_to_build {
        let mut pkg_deps: Vec<String> = Vec::new();
        if let Some(cached) = cache.get(&pkg.pkgbase) {
            // Global deps
            for dep in cached
                .srcinfo
                .depends
                .iter()
                .chain(cached.srcinfo.makedepends.iter())
                .chain(cached.srcinfo.checkdepends.iter())
            {
                let dep_name = DepResolver::dep_name(dep);
                if let Some(dep_pkgbase) = pkgname_to_pkgbase.get(dep_name)
                    && dep_pkgbase != &pkg.pkgbase
                    && pkgbases_set.contains(dep_pkgbase)
                {
                    pkg_deps.push(dep_pkgbase.clone());
                }
            }
            // Per-package deps
            for sp in &cached.srcinfo.packages {
                for dep in &sp.depends {
                    let dep_name = DepResolver::dep_name(dep);
                    if let Some(dep_pkgbase) = pkgname_to_pkgbase.get(dep_name)
                        && dep_pkgbase != &pkg.pkgbase
                        && pkgbases_set.contains(dep_pkgbase)
                    {
                        pkg_deps.push(dep_pkgbase.clone());
                    }
                }
            }
        }
        pkg_deps.sort();
        pkg_deps.dedup();
        deps_graph.insert(pkg.pkgbase.clone(), pkg_deps);
    }

    drop(cache);

    let build_order = crate::service::builds::topological_sort(&pkgbases_set, &deps_graph)?;

    let config = state.config.clone();

    // Create a shared cancel flag for the entire queue
    let cancel_flag = Arc::new(AtomicBool::new(false));

    let mut build_ids: Vec<String> = Vec::new();
    let mut pkg_info: Vec<(String, String, std::path::PathBuf)> = Vec::new();

    let now = chrono::Utc::now();

    for pkgbase in &build_order {
        let build_id = uuid::Uuid::new_v4().to_string();
        let log_filename = format!("{}.log", build_id);
        let log_path = config.resolved_cache().logs.join(&log_filename);

        // Get commit from cache
        let cache = state.srcinfo_cache.read().await;
        let commit = cache
            .get(pkgbase)
            .map(|c| c.commit.clone())
            .unwrap_or_else(|| {
                let clone_dir = state.config.resolved_cache().aur_clones.join(pkgbase);
                SrcInfoCache::get_head_commit(&clone_dir)
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            });
        drop(cache);

        let build = crate::models::Build {
            id: build_id.clone(),
            pkgbase: pkgbase.clone(),
            commit,
            status: crate::models::BuildStatus::Pending,
            started_at: now,
            finished_at: None,
            log_path: Some(log_filename),
            outputs: None,
            error_message: None,
        };

        state.db.insert_build(&build).await?;

        // Store cancel flag for each build
        {
            let mut flags = state.build_cancel_flags.write().await;
            flags.insert(build_id.clone(), cancel_flag.clone());
        }

        build_ids.push(build_id.clone());
        pkg_info.push((pkgbase.clone(), build_id, log_path));
    }

    let state_clone = state.clone();
    let deps_graph_clone = deps_graph.clone();
    let cancel_flag_clone = cancel_flag.clone();

    tokio::spawn(async move {
        crate::service::builds::run_queued_builds(
            state_clone,
            pkg_info,
            deps_graph_clone,
            config,
            cancel_flag_clone,
        )
        .await;
    });

    Ok(QueueBuildsResponse {
        build_order,
        build_ids,
        skipped,
    })
}

async fn cancel_build(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let build = state
        .db
        .get_build(&id)
        .await?
        .ok_or_else(|| AppError::not_found("Build not found"))?;

    let is_running = build.status == crate::models::BuildStatus::Running;
    let is_pending = build.status == crate::models::BuildStatus::Pending;

    if !is_running && !is_pending {
        return Err(AppError::bad_request("Build is not running or pending"));
    }

    // Set the cancel flag
    {
        let flags = state.build_cancel_flags.read().await;
        if let Some(flag) = flags.get(&id) {
            flag.store(true, Ordering::Relaxed);
        }
    }

    let pkgbase = build.pkgbase.clone();

    // Kill the container outside locks
    if is_running {
        let container_name = format!("repoman-build-{}", pkgbase);
        let build_system = BuildSystem::new(state.config.clone());
        build_system.stop_build(&container_name)?;
    }

    // Update DB state
    state
        .db
        .update_build_status(&id, crate::models::BuildStatus::Cancelled, None)
        .await?;
    // V2: No update_package_status

    Ok(StatusCode::OK)
}

// ==================== System handlers ====================

async fn repo_info(
    State(state): State<SharedState>,
) -> Result<Json<crate::repo::RepoStats>, AppError> {
    let stats = state.repo.stats()?;
    Ok(Json(stats))
}

async fn clean_repo(State(state): State<SharedState>) -> Result<Json<CleanResponse>, AppError> {
    let _repo_lock = state.repo_mutex.lock().await;
    let deleted = state
        .repo
        .clean_old_versions(state.config.repo.keep_versions)?;
    Ok(Json(CleanResponse {
        deleted_count: deleted.len() as u32,
        deleted_files: deleted,
    }))
}

async fn rebuild_repo_db(State(state): State<SharedState>) -> Result<StatusCode, AppError> {
    let _repo_lock = state.repo_mutex.lock().await;
    state.repo.rebuild_database()?;

    // Reload RepoState
    {
        let mut repo = state.repo_state.write().await;
        if let Err(e) = repo.reload() {
            tracing::error!("Failed to reload repo state after rebuild: {}", e);
        }
    }

    Ok(StatusCode::OK)
}

async fn remove_from_repo(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<RemoveFromRepoResponse>, AppError> {
    let pkg = crate::service::packages::resolve_package(&state, &name).await?;

    // Get pkgnames from srcinfo cache
    let cache = state.srcinfo_cache.read().await;
    let pkgnames: Vec<String> = cache
        .get(&pkg.pkgbase)
        .map(|c| {
            c.srcinfo
                .packages
                .iter()
                .map(|p| p.pkgname.clone())
                .collect()
        })
        .unwrap_or_default();
    drop(cache);

    // Remove from repo database
    let pkgname_refs: Vec<&str> = pkgnames.iter().map(|s| s.as_str()).collect();
    {
        let _repo_lock = state.repo_mutex.lock().await;
        let _ = state.build.repo_remove(&pkgname_refs);
    }

    let mut deleted_files = Vec::new();
    for pkg_name in &pkgnames {
        if let Ok(deleted) = state.repo.delete_package_files(pkg_name) {
            deleted_files.extend(deleted);
        }
    }

    // V2: No update_package_status -- status is derived

    // Reload RepoState
    {
        let mut repo = state.repo_state.write().await;
        if let Err(e) = repo.reload() {
            tracing::error!("Failed to reload repo state: {}", e);
        }
    }

    Ok(Json(RemoveFromRepoResponse {
        pkgbase: pkg.pkgbase,
        deleted_files,
    }))
}

async fn system_status(
    State(state): State<SharedState>,
) -> Result<Json<SystemStatusResponse>, AppError> {
    let podman = BuildSystem::check_podman().is_ok();
    let image = state.build.image_exists().unwrap_or(false);
    let repo = state.repo.stats().ok();

    let packages = state.db.list_packages().await?;

    let tracked_pkgbases: HashSet<String> = packages.iter().map(|p| p.pkgbase.clone()).collect();

    let mut counts = PackageCounts {
        total: packages.len() as u32,
        pending_review: 0,
        approved: 0,
        up_to_date: 0,
        update_available: 0,
        failed: 0,
    };

    for pkg in &packages {
        let status = derive_package_status(&state, pkg).await;
        match status {
            DerivedStatus::PendingReview | DerivedStatus::Missing => counts.pending_review += 1,
            DerivedStatus::Approved => counts.approved += 1,
            DerivedStatus::UpToDate => counts.up_to_date += 1,
            DerivedStatus::UpdateAvailable => counts.update_available += 1,
            DerivedStatus::Failed => counts.failed += 1,
            DerivedStatus::Building => counts.approved += 1, // count building as approved for status bar
        }
    }

    let pacman_cache = calculate_pacman_cache_stats(&state.config.resolved_cache().pacman_cache);
    let source_cache = calculate_source_cache_stats(&state.config.resolved_cache().sources);

    let known_log_files = state
        .db
        .list_build_log_filenames()
        .await
        .unwrap_or_default();
    let orphaned_data = detect_orphaned_data(&state.config, &tracked_pkgbases, &known_log_files);

    Ok(Json(SystemStatusResponse {
        podman,
        image,
        repo,
        packages: counts,
        pacman_cache,
        source_cache,
        orphaned_data,
        config: state.config.clone(),
    }))
}

async fn config_template() -> String {
    crate::config::Config::template()
}

fn calculate_pacman_cache_stats(cache_path: &std::path::Path) -> Option<PacmanCacheStats> {
    if !cache_path.exists() {
        return None;
    }

    let mut package_count = 0u32;
    let mut total_size = 0u64;

    if let Ok(entries) = std::fs::read_dir(cache_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz"))
            {
                package_count += 1;
                if let Ok(meta) = entry.metadata() {
                    total_size += meta.len();
                }
            }
        }
    }

    Some(PacmanCacheStats {
        path: cache_path.display().to_string(),
        package_count,
        total_size,
    })
}

fn calculate_source_cache_stats(sources_path: &std::path::Path) -> Option<CacheStats> {
    if !sources_path.exists() {
        return None;
    }

    let mut package_count = 0u32;
    let mut total_size = 0u64;

    if let Ok(entries) = std::fs::read_dir(sources_path) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                package_count += 1;
                if let Ok(files) = std::fs::read_dir(entry.path()) {
                    for file in files.flatten() {
                        if let Ok(meta) = file.metadata() {
                            total_size += meta.len();
                        }
                    }
                }
            }
        }
    }

    Some(CacheStats {
        path: sources_path.display().to_string(),
        package_count,
        total_size,
    })
}

async fn rebuild_image(State(state): State<SharedState>) -> Result<StatusCode, AppError> {
    state.build.build_base_image()?;
    Ok(StatusCode::OK)
}

async fn clean_cache(
    State(state): State<SharedState>,
) -> Result<Json<CleanCacheResponse>, AppError> {
    let cache_path = &state.config.resolved_cache().pacman_cache;
    let keep_versions = 2usize;
    let result = clean_pacman_cache(cache_path, keep_versions);
    Ok(Json(result))
}

fn parse_pacman_pkg_filename(path: &std::path::Path) -> Option<(String, String)> {
    let filename = path.file_name()?.to_str()?;

    let name = filename
        .strip_suffix(".pkg.tar.zst")
        .or_else(|| filename.strip_suffix(".pkg.tar.xz"))?;

    let parts: Vec<&str> = name.rsplitn(4, '-').collect();
    if parts.len() < 4 {
        return None;
    }

    let arch = parts[0];
    let pkgrel = parts[1];
    let pkgver = parts[2];
    let pkgname = parts[3];

    let version = format!("{}-{}-{}", pkgver, pkgrel, arch);
    Some((pkgname.to_string(), version))
}

fn clean_pacman_cache(cache_path: &std::path::Path, keep: usize) -> CleanCacheResponse {
    use std::collections::HashMap;

    let mut by_name: HashMap<String, Vec<(std::path::PathBuf, String)>> = HashMap::new();
    let mut result = CleanCacheResponse {
        packages_removed: 0,
        size_freed: 0,
    };

    if !cache_path.exists() {
        return result;
    }

    if let Ok(entries) = std::fs::read_dir(cache_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some((pkgname, version)) = parse_pacman_pkg_filename(&path) {
                by_name.entry(pkgname).or_default().push((path, version));
            }
        }
    }

    for (_name, mut versions) in by_name {
        versions.sort_by(|a, b| vercmp(&b.1, &a.1));

        for (path, _version) in versions.into_iter().skip(keep) {
            if let Ok(meta) = std::fs::metadata(&path) {
                result.size_freed += meta.len();
            }
            if std::fs::remove_file(&path).is_ok() {
                result.packages_removed += 1;
            }
        }
    }

    result
}

#[derive(Serialize)]
pub struct CleanCacheResponse {
    pub packages_removed: u32,
    pub size_freed: u64,
}

async fn clean_sources(
    State(state): State<SharedState>,
) -> Result<Json<CleanSourcesResponse>, AppError> {
    let sources_path = &state.config.resolved_cache().sources;

    let mut cleaned_count = 0u32;
    let mut cleaned_size = 0u64;

    if sources_path.exists()
        && let Ok(entries) = std::fs::read_dir(sources_path)
    {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Ok(files) = std::fs::read_dir(entry.path()) {
                    for file in files.flatten() {
                        if let Ok(meta) = file.metadata() {
                            cleaned_size += meta.len();
                        }
                    }
                }
                let path_str = entry.path().to_string_lossy().to_string();
                let status = std::process::Command::new("podman")
                    .args(["unshare", "rm", "-rf", &path_str])
                    .status();

                match status {
                    Ok(s) if s.success() => cleaned_count += 1,
                    _ => {
                        if std::fs::remove_dir_all(entry.path()).is_ok() {
                            cleaned_count += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(Json(CleanSourcesResponse {
        cleaned_count,
        cleaned_size,
    }))
}

#[derive(Serialize)]
pub struct CleanSourcesResponse {
    pub cleaned_count: u32,
    pub cleaned_size: u64,
}

fn detect_orphaned_data(
    config: &crate::config::Config,
    tracked_pkgbases: &HashSet<String>,
    known_log_files: &HashSet<String>,
) -> Option<OrphanedData> {
    let mut orphaned = OrphanedData {
        clones: Vec::new(),
        sources: Vec::new(),
        pkgdests: Vec::new(),
        logs: Vec::new(),
        total_size: 0,
    };

    if let Ok(entries) = std::fs::read_dir(&config.resolved_cache().aur_clones) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
                && !tracked_pkgbases.contains(name)
            {
                orphaned.total_size += dir_size(&entry.path());
                orphaned.clones.push(name.to_string());
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&config.resolved_cache().sources) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
                && !tracked_pkgbases.contains(name)
            {
                orphaned.total_size += dir_size(&entry.path());
                orphaned.sources.push(name.to_string());
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&config.resolved_cache().logs) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(pkgbase) = name.strip_prefix("pkgdest-") {
                    if !tracked_pkgbases.contains(pkgbase) {
                        orphaned.total_size += dir_size(&entry.path());
                        orphaned.pkgdests.push(pkgbase.to_string());
                    }
                } else if name.ends_with(".log") && !known_log_files.contains(name) {
                    if let Ok(meta) = entry.metadata() {
                        orphaned.total_size += meta.len();
                    }
                    orphaned.logs.push(name.to_string());
                }
            }
        }
    }

    if orphaned.clones.is_empty()
        && orphaned.sources.is_empty()
        && orphaned.pkgdests.is_empty()
        && orphaned.logs.is_empty()
    {
        return None;
    }

    Some(orphaned)
}

fn dir_size(path: &std::path::Path) -> u64 {
    let mut size = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                size += dir_size(&entry_path);
            } else if let Ok(meta) = entry.metadata() {
                size += meta.len();
            }
        }
    }
    size
}

async fn clean_orphans(
    State(state): State<SharedState>,
) -> Result<Json<CleanOrphansResponse>, AppError> {
    let packages = state.db.list_packages().await?;
    let tracked_pkgbases: HashSet<String> = packages.iter().map(|p| p.pkgbase.clone()).collect();

    let known_log_files = state
        .db
        .list_build_log_filenames()
        .await
        .unwrap_or_default();
    let orphaned = detect_orphaned_data(&state.config, &tracked_pkgbases, &known_log_files);

    let mut cleaned = CleanOrphansResponse {
        clones_removed: 0,
        sources_removed: 0,
        pkgdests_removed: 0,
        logs_removed: 0,
        total_size_freed: 0,
    };

    let Some(orphaned) = orphaned else {
        return Ok(Json(cleaned));
    };

    cleaned.total_size_freed = orphaned.total_size;

    for name in &orphaned.clones {
        let path = state.config.resolved_cache().aur_clones.join(name);
        if std::fs::remove_dir_all(&path).is_ok() {
            cleaned.clones_removed += 1;
        }
    }

    for name in &orphaned.sources {
        let path = state.config.resolved_cache().sources.join(name);
        let path_str = path.to_string_lossy().to_string();
        let status = std::process::Command::new("podman")
            .args(["unshare", "rm", "-rf", &path_str])
            .status();

        match status {
            Ok(s) if s.success() => cleaned.sources_removed += 1,
            _ => {
                if std::fs::remove_dir_all(&path).is_ok() {
                    cleaned.sources_removed += 1;
                }
            }
        }
    }

    for name in &orphaned.pkgdests {
        let path = state
            .config
            .resolved_cache()
            .logs
            .join(format!("pkgdest-{}", name));
        let path_str = path.to_string_lossy().to_string();
        let status = std::process::Command::new("podman")
            .args(["unshare", "rm", "-rf", &path_str])
            .status();

        match status {
            Ok(s) if s.success() => cleaned.pkgdests_removed += 1,
            _ => {
                if std::fs::remove_dir_all(&path).is_ok() {
                    cleaned.pkgdests_removed += 1;
                }
            }
        }
    }

    for name in &orphaned.logs {
        let path = state.config.resolved_cache().logs.join(name);
        if std::fs::remove_file(&path).is_ok() {
            cleaned.logs_removed += 1;
        }
    }

    Ok(Json(cleaned))
}

#[derive(Serialize)]
pub struct CleanOrphansResponse {
    pub clones_removed: u32,
    pub sources_removed: u32,
    pub pkgdests_removed: u32,
    pub logs_removed: u32,
    pub total_size_freed: u64,
}

// ==================== Container & Image Management ====================

async fn list_containers(
    State(state): State<SharedState>,
) -> Result<Json<Vec<crate::build::ContainerInfo>>, AppError> {
    let containers = state.build.list_containers()?;
    Ok(Json(containers))
}

async fn prune_containers(
    State(state): State<SharedState>,
) -> Result<Json<crate::build::PruneResult>, AppError> {
    let result = state.build.prune_containers()?;
    Ok(Json(result))
}

async fn list_images(
    State(state): State<SharedState>,
) -> Result<Json<Vec<crate::build::ImageInfo>>, AppError> {
    let images = state.build.list_images()?;
    Ok(Json(images))
}

#[derive(Deserialize)]
pub struct PruneImagesQuery {
    #[serde(default = "default_keep_images")]
    keep: usize,
}

fn default_keep_images() -> usize {
    3
}

async fn prune_images(
    State(state): State<SharedState>,
    Query(query): Query<PruneImagesQuery>,
) -> Result<Json<crate::build::PruneResult>, AppError> {
    let result = state.build.prune_images(query.keep)?;
    Ok(Json(result))
}
