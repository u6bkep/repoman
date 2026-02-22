use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt};
use tokio::sync::RwLock;

use crate::aur::{AurClient, AurGit, read_srcinfo};
use crate::build::{vercmp, BuildSystem};
use crate::config::Config;
use crate::db::Database;
use crate::models::{BuildStage, InstallReason, Package, PackageName, PackageStatus};
use crate::repo::RepoManager;

/// Shared application state
pub struct AppState {
    pub db: Database,
    pub config: Config,
    pub aur: AurClient,
    pub build: BuildSystem,
    pub repo: RepoManager,
    /// Current stage of running builds (in-memory tracking)
    pub build_stages: std::collections::HashMap<String, BuildStage>,
}

pub type SharedState = Arc<RwLock<AppState>>;

/// Create the API router
pub fn router(state: SharedState) -> Router {
    Router::new()
        // Package endpoints
        .route("/api/packages", get(list_packages).post(add_package))
        .route(
            "/api/packages/{pkgbase}",
            get(get_package).delete(remove_package).patch(update_package),
        )
        .route("/api/packages/{pkgbase}/pkgbuild", get(get_pkgbuild))
        .route("/api/packages/{pkgbase}/diff", get(get_diff))
        .route("/api/packages/{pkgbase}/approve", post(approve_package))
        .route("/api/packages/{pkgbase}/reject", post(reject_package))
        .route("/api/packages/{pkgbase}/build", post(trigger_build))
        .route("/api/packages/{pkgbase}/builds", get(list_builds))
        .route("/api/packages/{pkgbase}/mark", post(mark_package))
        .route("/api/packages/{pkgbase}/deps", get(get_package_deps))
        .route("/api/packages/{pkgbase}/check-update", post(check_package_update))
        .route("/api/packages/check-updates", post(check_updates))
        .route("/api/packages/orphans", get(list_orphans))
        // AUR endpoints
        .route("/api/aur/search", get(search_aur))
        .route("/api/aur/info/{name}", get(aur_info))
        // Build endpoints
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
        .with_state(state)
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
pub struct RejectRequest {
    pub reason: Option<String>,
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
    /// Packages that were removed
    pub removed: Vec<String>,
    /// Warning if other packages depend on the removed package(s)
    pub dependents_warning: Option<String>,
}

#[derive(Deserialize)]
pub struct PaginationQuery {
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

#[derive(Deserialize)]
pub struct ApproveQuery {
    pub commit: Option<String>,
}

#[derive(Serialize)]
pub struct PackageResponse {
    pub pkgbase: String,
    pub pkgnames: Vec<String>,
    pub pkgver: Option<String>,
    pub status: String,
    pub install_reason: String,
    pub approved_commit: Option<String>,
    pub current_commit: Option<String>,
    pub auto_approve: bool,
    /// Whether this package exists in AUR (false = removed/local only)
    pub has_upstream: bool,
    /// Whether this package exists in official repos (for detecting deprecated AUR packages)
    pub in_official_repos: bool,
    pub rejection_reason: Option<String>,
    /// Aggregated runtime dependencies from all pkgnames
    pub depends: Vec<String>,
    pub added_at: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct AddPackageResponse {
    #[serde(flatten)]
    pub package: PackageResponse,
    /// Dependencies not in official repos that may need to be added (if with_deps was false)
    pub missing_deps: Vec<crate::aur::MissingDep>,
    /// Dependencies that were automatically added (if with_deps was true)
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
    pub package_names: Vec<PackageName>,
    pub aur_info: Option<crate::models::AurPackageInfo>,
    pub builds: Vec<crate::models::Build>,
    /// Parsed .SRCINFO data (makedepends, sources, etc.)
    pub srcinfo: Option<crate::models::SrcInfo>,
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

#[derive(Serialize)]
pub struct BuildDetailResponse {
    #[serde(flatten)]
    pub build: crate::models::Build,
    /// Current stage of the build (for running builds)
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
    /// Build order (topologically sorted)
    pub build_order: Vec<String>,
    /// Build IDs for each package (in same order as build_order)
    pub build_ids: Vec<String>,
    /// Packages that were skipped (not approved or not found)
    pub skipped: Vec<QueueSkipped>,
}

#[derive(Serialize)]
pub struct QueueSkipped {
    pub pkgbase: String,
    pub reason: String,
}

#[derive(Serialize)]
pub struct DependencyInfo {
    pub name: String,
    pub version_constraint: Option<String>,
    pub source: String, // "official", "aur", "aur-tracked", "unknown"
    pub in_repo: bool,  // true if we're tracking this in our repo
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
    /// Orphaned AUR clone directories
    pub clones: Vec<String>,
    /// Orphaned source cache directories
    pub sources: Vec<String>,
    /// Orphaned pkgdest directories
    pub pkgdests: Vec<String>,
    /// Orphaned build logs
    pub logs: Vec<String>,
    /// Total size of all orphaned data
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
    message: String,
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
        (self.status, Json(ErrorResponse { error: self.message })).into_response()
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

fn package_to_response(pkg: Package, names: Vec<PackageName>) -> PackageResponse {
    // Aggregate dependencies from all package names
    let mut all_deps: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in &names {
        if let Some(deps_json) = &name.depends {
            if let Ok(deps) = serde_json::from_str::<Vec<String>>(deps_json) {
                all_deps.extend(deps);
            }
        }
    }
    let mut depends: Vec<String> = all_deps.into_iter().collect();
    depends.sort();

    PackageResponse {
        pkgbase: pkg.pkgbase,
        pkgnames: names.into_iter().map(|n| n.pkgname).collect(),
        pkgver: pkg.pkgver,
        status: pkg.status.to_string(),
        install_reason: pkg.install_reason.to_string(),
        approved_commit: pkg.approved_commit,
        current_commit: pkg.current_commit,
        auto_approve: pkg.auto_approve,
        has_upstream: pkg.has_upstream,
        in_official_repos: pkg.in_official_repos,
        rejection_reason: pkg.rejection_reason,
        depends,
        added_at: pkg.added_at.to_rfc3339(),
        updated_at: pkg.updated_at.to_rfc3339(),
    }
}

fn parse_status(s: &str) -> Result<PackageStatus, AppError> {
    match s {
        "pending_review" => Ok(PackageStatus::PendingReview),
        "approved" => Ok(PackageStatus::Approved),
        "building" => Ok(PackageStatus::Building),
        "failed" => Ok(PackageStatus::Failed),
        "up_to_date" => Ok(PackageStatus::UpToDate),
        "update_available" => Ok(PackageStatus::UpdateAvailable),
        "rejected" => Ok(PackageStatus::Rejected),
        _ => Err(AppError::bad_request(format!("Invalid status: {}", s))),
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

/// Resolve a name (pkgbase or pkgname) to a Package
async fn resolve_package(state: &AppState, name: &str) -> Result<Package, AppError> {
    // Try by pkgbase first
    if let Some(pkg) = state.db.get_package(name).await? {
        return Ok(pkg);
    }

    // Try by pkgname
    if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
        if let Some(pkg) = state.db.get_package(&pkgbase).await? {
            return Ok(pkg);
        }
    }

    Err(AppError::not_found("Package not found"))
}

// ==================== Package handlers ====================

async fn list_packages(
    State(state): State<SharedState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<PackageResponse>>, AppError> {
    let state = state.read().await;

    // Parse filters
    let status_filter = query.status.as_ref().map(|s| parse_status(s)).transpose()?;
    let reason_filter = query.reason.as_ref().map(|r| parse_reason(r)).transpose()?;

    // Get packages based on filters
    let packages = match (status_filter, reason_filter) {
        (Some(status), Some(reason)) => {
            // Both filters: get by status, then filter by reason in memory
            state
                .db
                .list_packages_by_status(status)
                .await?
                .into_iter()
                .filter(|p| p.install_reason == reason)
                .collect()
        }
        (Some(status), None) => state.db.list_packages_by_status(status).await?,
        (None, Some(reason)) => state.db.list_packages_by_reason(reason).await?,
        (None, None) => state.db.list_packages().await?,
    };

    // Fetch all package names in a single query to avoid N+1
    let mut all_names = state.db.list_all_package_names().await?;

    let responses = packages
        .into_iter()
        .map(|pkg| {
            let names = all_names.remove(&pkg.pkgbase).unwrap_or_default();
            package_to_response(pkg, names)
        })
        .collect();

    Ok(Json(responses))
}

async fn add_package(
    State(state): State<SharedState>,
    Json(req): Json<AddPackageRequest>,
) -> Result<Json<AddPackageResponse>, AppError> {
    let mut state = state.write().await;

    // Check if package already exists (by pkgbase or pkgname)
    if let Some(pkg) = state.db.get_package(&req.name).await? {
        let names = state.db.get_package_names(&pkg.pkgbase).await?;
        return Ok(Json(AddPackageResponse {
            package: package_to_response(pkg, names),
            missing_deps: vec![],
            added_deps: vec![],
        }));
    }

    if let Some(pkgbase) = state.db.resolve_pkgbase(&req.name).await? {
        if let Some(pkg) = state.db.get_package(&pkgbase).await? {
            let names = state.db.get_package_names(&pkg.pkgbase).await?;
            return Ok(Json(AddPackageResponse {
                package: package_to_response(pkg, names),
                missing_deps: vec![],
                added_deps: vec![],
            }));
        }
    }

    // Determine install reason for the main package
    let install_reason = if req.as_dependency {
        InstallReason::Dependency
    } else {
        InstallReason::Explicit
    };

    // Add the main package
    let (pkg, _version) = add_package_internal(&mut state, &req.name, install_reason).await?;

    // Track added dependencies
    let mut added_deps: Vec<AddedDependency> = Vec::new();

    // Find missing AUR dependencies
    let clone_path = std::path::Path::new(&pkg.local_path);
    let srcinfo = read_srcinfo(clone_path)?;

    // Get currently tracked package names (for dep resolution)
    let mut tracked_pkgnames = get_tracked_pkgnames(&state).await?;

    let container_image = state.config.container.image.clone();
    let missing_deps = state
        .aur
        .find_missing_aur_deps(&srcinfo, &tracked_pkgnames, &container_image)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to check for missing deps: {}", e);
            vec![]
        });

    // If with_deps is true, recursively add missing AUR dependencies
    let remaining_missing_deps = if req.with_deps && !missing_deps.is_empty() {
        // Track which packages we've processed to avoid cycles
        let mut processed: std::collections::HashSet<String> = std::collections::HashSet::new();
        processed.insert(pkg.pkgbase.clone());

        // Queue of packages to check for deps (pkgbase, name for AUR lookup)
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

            // Add this dependency
            match add_package_internal(&mut state, &dep_name, InstallReason::Dependency).await {
                Ok((dep_pkg, dep_version)) => {
                    added_deps.push(AddedDependency {
                        pkgbase: dep_pkg.pkgbase.clone(),
                        version: dep_version,
                    });

                    // Check for this package's dependencies
                    let dep_clone_path = std::path::Path::new(&dep_pkg.local_path);
                    if let Ok(dep_srcinfo) = read_srcinfo(dep_clone_path) {
                        // Update tracked pkgnames
                        tracked_pkgnames = get_tracked_pkgnames(&state).await?;

                        if let Ok(dep_missing) = state
                            .aur
                            .find_missing_aur_deps(&dep_srcinfo, &tracked_pkgnames, &container_image)
                            .await
                        {
                            for m in dep_missing {
                                if m.in_aur {
                                    if let Some(pb) = &m.pkgbase {
                                        if !processed.contains(pb) {
                                            to_check.push((pb.clone(), m.name.clone()));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to add dependency {}: {}", dep_name, e.message);
                }
            }
        }

        // Return empty missing_deps since we added them
        vec![]
    } else {
        missing_deps
    };

    let names = state.db.get_package_names(&pkg.pkgbase).await?;
    Ok(Json(AddPackageResponse {
        package: package_to_response(pkg, names),
        missing_deps: remaining_missing_deps,
        added_deps,
    }))
}

/// Helper to get all tracked package names
async fn get_tracked_pkgnames(
    state: &AppState,
) -> Result<std::collections::HashSet<String>, AppError> {
    let tracked_packages = state.db.list_packages().await?;
    let mut tracked_pkgnames: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in &tracked_packages {
        tracked_pkgnames.insert(p.pkgbase.clone());
        let names = state.db.get_package_names(&p.pkgbase).await?;
        for n in names {
            tracked_pkgnames.insert(n.pkgname);
        }
    }
    Ok(tracked_pkgnames)
}

/// Internal helper to add a single package
async fn add_package_internal(
    state: &mut AppState,
    name: &str,
    install_reason: InstallReason,
) -> Result<(Package, String), AppError> {
    // Check if already exists
    if let Some(pkg) = state.db.get_package(name).await? {
        let version = pkg.pkgver.clone().unwrap_or_default();
        return Ok((pkg, version));
    }
    if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
        if let Some(pkg) = state.db.get_package(&pkgbase).await? {
            let version = pkg.pkgver.clone().unwrap_or_default();
            return Ok((pkg, version));
        }
    }

    // Query AUR
    let aur_info = state
        .aur
        .info_single(name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Package '{}' not found in AUR", name)))?;

    let pkgbase = &aur_info.package_base;

    // Check again after AUR lookup
    if let Some(pkg) = state.db.get_package(pkgbase).await? {
        let version = pkg.pkgver.clone().unwrap_or_default();
        return Ok((pkg, version));
    }

    // Clone repository
    let clone_path = state.config.resolved_cache().aur_clones.join(pkgbase);
    if !clone_path.exists() {
        AurGit::clone(pkgbase, &clone_path)?;
    }

    // Parse .SRCINFO
    let srcinfo = read_srcinfo(&clone_path)?;
    let current_commit = AurGit::head_commit(&clone_path)?;
    let version = format!("{}-{}", srcinfo.pkgver, srcinfo.pkgrel);

    // Create package record
    let now = chrono::Utc::now();
    let pkg = Package {
        pkgbase: pkgbase.clone(),
        aur_url: format!("https://aur.archlinux.org/{}.git", pkgbase),
        local_path: clone_path.to_string_lossy().to_string(),
        approved_commit: None,
        current_commit: Some(current_commit),
        pkgver: Some(version.clone()),
        status: PackageStatus::PendingReview,
        install_reason,
        auto_approve: false,
        has_upstream: true,
        in_official_repos: false,
        rejection_reason: None,
        added_at: now,
        updated_at: now,
    };

    // Build package names list
    let mut pkg_names = Vec::new();
    for sp in &srcinfo.packages {
        pkg_names.push(PackageName {
            pkgname: sp.pkgname.clone(),
            pkgbase: pkgbase.clone(),
            pkgdesc: sp.pkgdesc.clone().or(srcinfo.pkgdesc.clone()),
            pkgver: Some(version.clone()),
            depends: Some(serde_json::to_string(&sp.depends).unwrap_or_default()),
            optdepends: Some(serde_json::to_string(&sp.optdepends).unwrap_or_default()),
            provides: Some(serde_json::to_string(&sp.provides).unwrap_or_default()),
            conflicts: Some(serde_json::to_string(&sp.conflicts).unwrap_or_default()),
            repo_version: None,
        });
    }

    // Insert package and its names atomically
    state.db.insert_package_with_names(&pkg, &pkg_names).await?;

    Ok((pkg, version))
}

async fn get_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PackageDetailResponse>, AppError> {
    let state = state.read().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    let names = state.db.get_package_names(&pkg.pkgbase).await?;
    let aur_info = state.aur.info_single(&pkg.pkgbase).await?;
    let builds = state.db.list_builds(&pkg.pkgbase, 10, 0).await?;

    // Read .SRCINFO if the clone exists
    let clone_path = std::path::Path::new(&pkg.local_path);
    let srcinfo = if clone_path.exists() {
        read_srcinfo(clone_path).ok()
    } else {
        None
    };

    Ok(Json(PackageDetailResponse {
        package: package_to_response(pkg, names.clone()),
        package_names: names,
        aur_info,
        builds,
        srcinfo,
    }))
}

async fn remove_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Query(params): Query<DeleteParams>,
) -> Result<Json<DeletePackageResponse>, AppError> {
    let mut state = state.write().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    let pkg_names = state.db.get_package_names(&pkg.pkgbase).await?;
    let pkgnames_set: std::collections::HashSet<String> = pkg_names
        .iter()
        .map(|n| n.pkgname.clone())
        .collect();

    // Check if any other tracked packages depend on this package
    let all_packages = state.db.list_packages().await?;
    let mut dependents: Vec<String> = Vec::new();
    for other_pkg in &all_packages {
        if other_pkg.pkgbase == pkg.pkgbase {
            continue;
        }
        let other_names = state.db.get_package_names(&other_pkg.pkgbase).await?;
        for name in &other_names {
            if let Some(deps_json) = &name.depends {
                if let Ok(deps) = serde_json::from_str::<Vec<String>>(deps_json) {
                    for dep in deps {
                        let dep_name = crate::aur::DepResolver::dep_name(&dep);
                        if pkgnames_set.contains(dep_name) {
                            dependents.push(other_pkg.pkgbase.clone());
                            break;
                        }
                    }
                }
            }
        }
    }
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

    // Helper to remove a single package
    async fn do_remove(
        state: &mut AppState,
        pkgbase: &str,
        delete_files: bool,
        delete_clone: bool,
    ) -> Result<(), AppError> {
        let pkg = state
            .db
            .get_package(pkgbase)
            .await?
            .ok_or_else(|| AppError::not_found("Package not found"))?;

        if delete_files {
            let names = state.db.get_package_names(pkgbase).await?;
            let pkgnames: Vec<&str> = names.iter().map(|n| n.pkgname.as_str()).collect();
            let _ = state.build.repo_remove(&pkgnames); // Ignore errors
            for name in &names {
                let _ = state.repo.delete_package_files(&name.pkgname);
            }
        }

        if delete_clone {
            let clone_path = std::path::Path::new(&pkg.local_path);
            if clone_path.exists() {
                let _ = std::fs::remove_dir_all(clone_path);
            }
        }

        state.db.delete_package(pkgbase).await?;
        Ok(())
    }

    let delete_files = params.delete_files.unwrap_or(false);
    let delete_clone = params.delete_clone.unwrap_or(false);
    let recursive = params.recursive.unwrap_or(false);

    // Remove the main package
    let mut removed = vec![pkg.pkgbase.clone()];
    do_remove(&mut state, &pkg.pkgbase, delete_files, delete_clone).await?;

    // If recursive, also remove orphaned dependencies
    if recursive {
        loop {
            let orphans = state.db.list_orphans().await?;
            if orphans.is_empty() {
                break;
            }
            for orphan in orphans {
                removed.push(orphan.pkgbase.clone());
                do_remove(&mut state, &orphan.pkgbase, delete_files, delete_clone).await?;
            }
        }
    }

    Ok(Json(DeletePackageResponse {
        removed,
        dependents_warning,
    }))
}

async fn get_pkgbuild(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PkgbuildResponse>, AppError> {
    let state = state.read().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    let clone_path = std::path::Path::new(&pkg.local_path);
    let pkgbuild_path = clone_path.join("PKGBUILD");

    let content = std::fs::read_to_string(&pkgbuild_path)?;
    let commit = AurGit::head_commit(clone_path)?;

    // List all files in the repo
    let mut files = Vec::new();
    for entry in std::fs::read_dir(clone_path)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if !name.starts_with('.') || name == ".SRCINFO" {
                files.push(name.to_string());
            }
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
    let state = state.read().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    let clone_path = std::path::Path::new(&pkg.local_path);

    let diff = if let Some(ref approved) = pkg.approved_commit {
        let current = pkg.current_commit.as_deref().unwrap_or("HEAD");
        AurGit::diff(clone_path, approved, current)?
    } else {
        // No approved commit, show full PKGBUILD as "new"
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
        to_commit: pkg.current_commit,
        diff,
    }))
}

async fn approve_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Query(query): Query<ApproveQuery>,
) -> Result<StatusCode, AppError> {
    let state = state.write().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    state
        .db
        .approve_package(&pkg.pkgbase, false, query.commit.as_deref())
        .await?;

    Ok(StatusCode::OK)
}

async fn reject_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Json(req): Json<RejectRequest>,
) -> Result<StatusCode, AppError> {
    let state = state.write().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    state
        .db
        .reject_package(&pkg.pkgbase, req.reason.as_deref())
        .await?;

    Ok(StatusCode::OK)
}

async fn trigger_build(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<BuildResponse>, AppError> {
    // Get package info and config under read lock first
    let (pkg, config, log_path, build_id) = {
        let state_read = state.read().await;

        let pkg = resolve_package(&state_read, &pkgbase).await?;

        if pkg.status != PackageStatus::Approved && pkg.status != PackageStatus::UpdateAvailable {
            return Err(AppError::bad_request(
                "Package must be approved before building",
            ));
        }

        // Check base image exists
        match state_read.build.image_exists() {
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

        let build_id = uuid::Uuid::new_v4().to_string();
        let log_path = state_read.config.resolved_cache().logs.join(format!("{}.log", build_id));

        (pkg, state_read.config.clone(), log_path, build_id)
    };

    let commit = pkg
        .current_commit
        .clone()
        .ok_or_else(|| AppError::bad_request("Package has no current commit"))?;

    // Create build record and update package status
    {
        let state_write = state.write().await;
        let now = chrono::Utc::now();

        let build = crate::models::Build {
            id: build_id.clone(),
            pkgbase: pkg.pkgbase.clone(),
            commit,
            status: crate::models::BuildStatus::Pending,
            started_at: now,
            finished_at: None,
            log_path: Some(log_path.to_string_lossy().to_string()),
            outputs: None,
            error_message: None,
        };

        state_write.db.insert_build(&build).await?;
        state_write
            .db
            .update_package_status(&pkg.pkgbase, PackageStatus::Building)
            .await?;
    }

    // Spawn background task to run the build
    let state_clone = state.clone();
    let pkgbase_clone = pkg.pkgbase.clone();
    let local_path = pkg.local_path.clone();
    let build_id_clone = build_id.clone();

    tokio::spawn(async move {
        run_build_task(
            state_clone,
            pkgbase_clone,
            build_id_clone,
            local_path,
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

/// Helper to update build stage in memory
async fn set_build_stage(state: &SharedState, build_id: &str, stage: BuildStage) {
    let mut state_write = state.write().await;
    state_write.build_stages.insert(build_id.to_string(), stage);
}

/// Helper to remove build stage from memory (when build completes)
async fn clear_build_stage(state: &SharedState, build_id: &str) {
    let mut state_write = state.write().await;
    state_write.build_stages.remove(build_id);
}

/// Background task that executes a build
async fn run_build_task(
    state: SharedState,
    pkgbase: String,
    build_id: String,
    local_path: String,
    log_path: std::path::PathBuf,
    config: crate::config::Config,
) {
    tracing::info!("Starting build task for {} (build {})", pkgbase, build_id);

    // Initialize stage tracking
    set_build_stage(&state, &build_id, BuildStage::Starting).await;

    // Update status to Running
    {
        let state_write = state.write().await;
        if let Err(e) = state_write
            .db
            .update_build_status(&build_id, crate::models::BuildStatus::Running, None)
            .await
        {
            tracing::error!("Failed to update build status to Running: {}", e);
            clear_build_stage(&state, &build_id).await;
            return;
        }
    }

    // Create build system and ensure repo exists
    let build_system = BuildSystem::new(config.clone());

    if let Err(e) = std::fs::create_dir_all(&config.repo.path) {
        tracing::error!("Failed to create repo directory: {}", e);
        update_build_failed(
            &state,
            &build_id,
            &pkgbase,
            &format!("Failed to create repo dir: {}", e),
        )
        .await;
        return;
    }

    if let Err(e) = build_system.init_repo() {
        tracing::error!("Failed to init repo: {}", e);
        update_build_failed(
            &state,
            &build_id,
            &pkgbase,
            &format!("Failed to init repo: {}", e),
        )
        .await;
        return;
    }

    // Run the build using spawn_blocking for the sync version
    let source_path = std::path::PathBuf::from(&local_path);
    let log_path_clone = log_path.clone();
    let pkgbase_clone = pkgbase.clone();

    let build_result = tokio::task::spawn_blocking(move || {
        build_system.run_build_sync(&pkgbase_clone, &source_path, &log_path_clone)
    })
    .await;

    match build_result {
        Ok(Ok(packages)) => {
            // Build succeeded - move packages to repo
            tracing::info!(
                "Build succeeded for {}, moving {} package(s) to repo",
                pkgbase,
                packages.len()
            );

            set_build_stage(&state, &build_id, BuildStage::MovingToRepo).await;

            let build_system = BuildSystem::new(config.clone());

            match build_system.move_to_repo(&packages) {
                Ok(repo_packages) => {
                    // Add to repo database
                    set_build_stage(&state, &build_id, BuildStage::UpdatingIndex).await;

                    if let Err(e) = build_system.repo_add(&repo_packages) {
                        tracing::error!("repo-add failed: {}", e);
                        set_build_stage(&state, &build_id, BuildStage::Failed).await;
                        update_build_failed(
                            &state,
                            &build_id,
                            &pkgbase,
                            &format!("repo-add failed: {}", e),
                        )
                        .await;
                        clear_build_stage(&state, &build_id).await;
                        return;
                    }

                    // Update build record with outputs
                    let outputs: Vec<String> = repo_packages
                        .iter()
                        .filter_map(|p| p.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .collect();
                    let outputs_json = match serde_json::to_string(&outputs) {
                        Ok(json) => json,
                        Err(e) => {
                            tracing::error!("Failed to serialize build outputs: {}", e);
                            "[]".to_string()
                        }
                    };

                    set_build_stage(&state, &build_id, BuildStage::Complete).await;

                    let state_write = state.write().await;
                    if let Err(e) = state_write
                        .db
                        .update_build_status(&build_id, crate::models::BuildStatus::Success, None)
                        .await
                    {
                        tracing::error!(
                            "Failed to update build {} status to Success: {}",
                            build_id,
                            e
                        );
                    }
                    if let Err(e) = state_write
                        .db
                        .update_build_outputs(&build_id, &outputs_json)
                        .await
                    {
                        tracing::error!("Failed to update build {} outputs: {}", build_id, e);
                    }
                    if let Err(e) = state_write
                        .db
                        .update_package_status(&pkgbase, PackageStatus::UpToDate)
                        .await
                    {
                        tracing::error!(
                            "Failed to update package {} status to UpToDate: {}",
                            pkgbase,
                            e
                        );
                    }
                    drop(state_write);

                    clear_build_stage(&state, &build_id).await;
                    tracing::info!("Build {} completed successfully", build_id);
                }
                Err(e) => {
                    set_build_stage(&state, &build_id, BuildStage::Failed).await;
                    update_build_failed(
                        &state,
                        &build_id,
                        &pkgbase,
                        &format!("Failed to move packages: {}", e),
                    )
                    .await;
                    clear_build_stage(&state, &build_id).await;
                }
            }
        }
        Ok(Err(e)) => {
            // Build command failed
            tracing::error!("Build failed for {}: {}", pkgbase, e);
            set_build_stage(&state, &build_id, BuildStage::Failed).await;
            update_build_failed(&state, &build_id, &pkgbase, &e.to_string()).await;
            clear_build_stage(&state, &build_id).await;
        }
        Err(e) => {
            // Task panicked
            tracing::error!("Build task panicked for {}: {}", pkgbase, e);
            set_build_stage(&state, &build_id, BuildStage::Failed).await;
            update_build_failed(
                &state,
                &build_id,
                &pkgbase,
                &format!("Build task panicked: {}", e),
            )
            .await;
            clear_build_stage(&state, &build_id).await;
        }
    }
}

/// Helper to update build and package status on failure
async fn update_build_failed(state: &SharedState, build_id: &str, pkgbase: &str, error: &str) {
    tracing::error!("Build {} failed: {}", build_id, error);
    let state_write = state.write().await;
    if let Err(e) = state_write
        .db
        .update_build_status(build_id, crate::models::BuildStatus::Failed, Some(error))
        .await
    {
        tracing::error!("Failed to update build {} status to Failed: {}", build_id, e);
    }
    if let Err(e) = state_write
        .db
        .update_package_status(pkgbase, PackageStatus::Failed)
        .await
    {
        tracing::error!(
            "Failed to update package {} status to Failed: {}",
            pkgbase,
            e
        );
    }
}

async fn list_builds(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Query(pagination): Query<PaginationQuery>,
) -> Result<Json<Vec<crate::models::Build>>, AppError> {
    let state = state.read().await;

    let limit = pagination.limit.unwrap_or(20).min(100); // Default 20, max 100
    let offset = pagination.offset.unwrap_or(0).max(0);

    let pkg = resolve_package(&state, &pkgbase).await?;
    let builds = state.db.list_builds(&pkg.pkgbase, limit, offset).await?;

    Ok(Json(builds))
}

async fn mark_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Json(req): Json<MarkRequest>,
) -> Result<Json<PackageResponse>, AppError> {
    let state = state.write().await;

    let pkg = resolve_package(&state, &pkgbase).await?;

    let reason: InstallReason = req
        .install_reason
        .parse()
        .map_err(|e: String| AppError::bad_request(e))?;

    state.db.update_install_reason(&pkg.pkgbase, reason).await?;

    // Reload package to get updated data
    let pkg = state
        .db
        .get_package(&pkg.pkgbase)
        .await?
        .ok_or_else(|| AppError::internal("Package disappeared"))?;
    let names = state.db.get_package_names(&pkg.pkgbase).await?;

    Ok(Json(package_to_response(pkg, names)))
}

async fn update_package(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
    Json(req): Json<UpdatePackageRequest>,
) -> Result<Json<PackageResponse>, AppError> {
    let state = state.write().await;

    let pkg = resolve_package(&state, &pkgbase).await?;

    // Update auto_approve if provided
    if let Some(auto_approve) = req.auto_approve {
        state
            .db
            .update_auto_approve(&pkg.pkgbase, auto_approve)
            .await?;
    }

    // Reload package to get updated data
    let pkg = state
        .db
        .get_package(&pkg.pkgbase)
        .await?
        .ok_or_else(|| AppError::internal("Package disappeared"))?;
    let names = state.db.get_package_names(&pkg.pkgbase).await?;

    Ok(Json(package_to_response(pkg, names)))
}

async fn list_orphans(
    State(state): State<SharedState>,
) -> Result<Json<Vec<PackageResponse>>, AppError> {
    let state = state.read().await;

    let orphans = state.db.list_orphans().await?;

    // Fetch all package names in a single query to avoid N+1
    let mut all_names = state.db.list_all_package_names().await?;

    let responses = orphans
        .into_iter()
        .map(|pkg| {
            let names = all_names.remove(&pkg.pkgbase).unwrap_or_default();
            package_to_response(pkg, names)
        })
        .collect();

    Ok(Json(responses))
}

async fn get_package_deps(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PackageDepsResponse>, AppError> {
    let state = state.read().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    let clone_path = std::path::Path::new(&pkg.local_path);

    // Parse .SRCINFO
    let srcinfo = read_srcinfo(clone_path)?;

    // Get list of packages we're tracking
    let tracked_packages = state.db.list_packages().await?;
    let tracked_names: std::collections::HashSet<String> = tracked_packages
        .iter()
        .map(|p| p.pkgbase.clone())
        .collect();

    // Also get all pkgnames we track
    let mut tracked_pkgnames: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tp in &tracked_packages {
        let names = state.db.get_package_names(&tp.pkgbase).await?;
        for name in names {
            tracked_pkgnames.insert(name.pkgname);
        }
    }

    // Helper to parse dep string like "package>=1.0" into (name, constraint)
    fn parse_dep(dep: &str) -> (String, Option<String>) {
        // Find first version operator
        for op in &[">=", "<=", "=", ">", "<"] {
            if let Some(pos) = dep.find(op) {
                return (dep[..pos].to_string(), Some(dep[pos..].to_string()));
            }
        }
        (dep.to_string(), None)
    }

    // Helper to classify a dependency
    async fn classify_dep(
        name: &str,
        aur: &crate::aur::AurClient,
        tracked_names: &std::collections::HashSet<String>,
        tracked_pkgnames: &std::collections::HashSet<String>,
    ) -> (String, bool) {
        // Check if we're tracking it
        let in_repo = tracked_names.contains(name) || tracked_pkgnames.contains(name);

        // Check if it's in official repos using pacman
        let pacman_check = std::process::Command::new("pacman")
            .args(["-Si", name])
            .output();

        if let Ok(output) = pacman_check {
            if output.status.success() {
                return ("official".to_string(), in_repo);
            }
        }

        // Check if it's in AUR
        if let Ok(Some(_)) = aur.info_single(name).await {
            if in_repo {
                return ("aur-tracked".to_string(), true);
            }
            return ("aur".to_string(), false);
        }

        // Could be a virtual package (provides) - check with pacman -Ss
        let pacman_search = std::process::Command::new("pacman")
            .args(["-Ss", &format!("^{}$", name)])
            .output();

        if let Ok(output) = pacman_search {
            if output.status.success() && !output.stdout.is_empty() {
                return ("official".to_string(), in_repo);
            }
        }

        ("unknown".to_string(), in_repo)
    }

    // Process depends
    let mut depends = Vec::new();
    for dep in &srcinfo.depends {
        let (name, constraint) = parse_dep(dep);
        let (source, in_repo) = classify_dep(&name, &state.aur, &tracked_names, &tracked_pkgnames).await;
        depends.push(DependencyInfo {
            name,
            version_constraint: constraint,
            source,
            in_repo,
        });
    }

    // Process makedepends
    let mut makedepends = Vec::new();
    for dep in &srcinfo.makedepends {
        let (name, constraint) = parse_dep(dep);
        let (source, in_repo) = classify_dep(&name, &state.aur, &tracked_names, &tracked_pkgnames).await;
        makedepends.push(DependencyInfo {
            name,
            version_constraint: constraint,
            source,
            in_repo,
        });
    }

    // Process checkdepends
    let mut checkdepends = Vec::new();
    for dep in &srcinfo.checkdepends {
        let (name, constraint) = parse_dep(dep);
        let (source, in_repo) = classify_dep(&name, &state.aur, &tracked_names, &tracked_pkgnames).await;
        checkdepends.push(DependencyInfo {
            name,
            version_constraint: constraint,
            source,
            in_repo,
        });
    }

    Ok(Json(PackageDepsResponse {
        depends,
        makedepends,
        checkdepends,
        optdepends: srcinfo.optdepends,
    }))
}

async fn check_updates(
    State(state): State<SharedState>,
) -> Result<Json<CheckUpdatesResponse>, AppError> {
    let state = state.write().await;

    let packages = state.db.list_packages().await?;
    let checked = packages.len();
    let mut updated = Vec::new();

    for pkg in packages {
        // Don't touch the source directory while a build is actively using it
        if pkg.status == PackageStatus::Building {
            continue;
        }

        let clone_path = std::path::Path::new(&pkg.local_path);

        if AurGit::has_updates(clone_path)? {
            AurGit::reset_to_remote(clone_path)?;
            let new_commit = AurGit::head_commit(clone_path)?;

            // Re-parse .SRCINFO
            let srcinfo = read_srcinfo(clone_path)?;
            let pkgver = format!("{}-{}", srcinfo.pkgver, srcinfo.pkgrel);

            state
                .db
                .update_package_commit(&pkg.pkgbase, &new_commit, Some(&pkgver))
                .await?;

            // Auto-approve only if the package already had an initial human review
            // (i.e. status is UpdateAvailable, not PendingReview).
            // PendingReview packages must be reviewed by a human first.
            let new_status = if pkg.auto_approve && pkg.status == PackageStatus::UpdateAvailable {
                state.db.approve_package(&pkg.pkgbase, true, None).await?;
                PackageStatus::Approved
            } else {
                PackageStatus::UpdateAvailable
            };

            state
                .db
                .update_package_status(&pkg.pkgbase, new_status)
                .await?;
            updated.push(pkg.pkgbase);
        }
    }

    Ok(Json(CheckUpdatesResponse { checked, updated }))
}

async fn check_package_update(
    State(state): State<SharedState>,
    Path(pkgbase): Path<String>,
) -> Result<Json<PackageResponse>, AppError> {
    let state = state.write().await;

    let pkg = resolve_package(&state, &pkgbase).await?;
    let clone_path = std::path::Path::new(&pkg.local_path).to_path_buf();

    if AurGit::has_updates(&clone_path)? {
        AurGit::reset_to_remote(&clone_path)?;
        let new_commit = AurGit::head_commit(&clone_path)?;

        // Re-parse .SRCINFO
        let srcinfo = read_srcinfo(&clone_path)?;
        let pkgver = format!("{}-{}", srcinfo.pkgver, srcinfo.pkgrel);

        state
            .db
            .update_package_commit(&pkg.pkgbase, &new_commit, Some(&pkgver))
            .await?;

        state
            .db
            .update_package_status(&pkg.pkgbase, PackageStatus::UpdateAvailable)
            .await?;
    }

    // Reload package to get updated data
    let pkg = state
        .db
        .get_package(&pkg.pkgbase)
        .await?
        .ok_or_else(|| AppError::internal("Package disappeared"))?;
    let names = state.db.get_package_names(&pkg.pkgbase).await?;

    Ok(Json(package_to_response(pkg, names)))
}

// ==================== AUR handlers ====================

async fn search_aur(
    State(state): State<SharedState>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<crate::models::AurSearchResult>>, AppError> {
    let state = state.read().await;
    let results = state.aur.search(&query.q).await?;

    // Get the set of tracked pkgbases
    let tracked_pkgbases: std::collections::HashSet<String> = state
        .db
        .list_packages()
        .await?
        .into_iter()
        .map(|p| p.pkgbase)
        .collect();

    // Annotate each result with whether it's tracked
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
    let state = state.read().await;
    let info = state.aur.info_single(&name).await?;
    Ok(Json(info))
}

// ==================== Build handlers ====================

async fn get_build(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<BuildDetailResponse>, AppError> {
    let state = state.read().await;
    let build = state
        .db
        .get_build(&id)
        .await?
        .ok_or_else(|| AppError::not_found("Build not found"))?;

    // Get current stage from in-memory tracking (only for running builds)
    let current_stage = if build.status == crate::models::BuildStatus::Running
        || build.status == crate::models::BuildStatus::Pending
    {
        state.build_stages.get(&id).map(|s| s.to_string())
    } else {
        None
    };

    Ok(Json(BuildDetailResponse {
        build,
        current_stage,
    }))
}

async fn get_build_logs(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<String, AppError> {
    let state = state.read().await;
    let build = state
        .db
        .get_build(&id)
        .await?
        .ok_or_else(|| AppError::not_found("Build not found"))?;

    let log_path = build
        .log_path
        .ok_or_else(|| AppError::not_found("No log file for this build"))?;

    let content = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|_| "Log file not found or empty".to_string());

    Ok(content)
}

/// WebSocket endpoint for streaming build logs in real-time
async fn stream_build_logs(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, AppError> {
    // Validate build exists and get log path
    let (log_path, build_id) = {
        let state_read = state.read().await;
        let build = state_read
            .db
            .get_build(&id)
            .await?
            .ok_or_else(|| AppError::not_found("Build not found"))?;

        let log_path = build
            .log_path
            .ok_or_else(|| AppError::not_found("No log file for this build"))?;

        (std::path::PathBuf::from(log_path), build.id)
    };

    Ok(ws.on_upgrade(move |socket| handle_log_stream(socket, state, build_id, log_path)))
}

/// WebSocket message for stage changes
#[derive(Serialize)]
struct StageChangeMessage {
    #[serde(rename = "type")]
    msg_type: &'static str,
    stage: String,
}

/// Send a stage change message over WebSocket
async fn send_stage_change(socket: &mut WebSocket, stage: BuildStage) -> bool {
    let msg = StageChangeMessage {
        msg_type: "stage",
        stage: stage.to_string(),
    };
    if let Ok(json) = serde_json::to_string(&msg) {
        socket.send(Message::Text(json.into())).await.is_ok()
    } else {
        true // Don't disconnect on serialization failure
    }
}

/// Handle the WebSocket connection for log streaming
async fn handle_log_stream(
    mut socket: WebSocket,
    state: SharedState,
    build_id: String,
    log_path: std::path::PathBuf,
) {
    tracing::info!("WebSocket connected for build {} logs", build_id);

    // Track the last stage we sent to avoid duplicates
    let mut last_sent_stage: Option<BuildStage> = None;

    // Send the current stage if build is already in progress
    {
        let state_read = state.read().await;
        if let Some(&stage) = state_read.build_stages.get(&build_id) {
            if send_stage_change(&mut socket, stage).await {
                last_sent_stage = Some(stage);
            }
        }
    }

    // Wait for log file to exist (build might just be starting)
    let mut attempts = 0;
    while !log_path.exists() && attempts < 30 {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        attempts += 1;
    }

    // Open the log file
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
        // Read any new lines
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break, // No more data available right now
                Ok(_) => {
                    // Check for stage change markers in the log line
                    if let Some(new_stage) = BuildStage::from_log_line(&line_buf) {
                        // Update in-memory stage tracking
                        set_build_stage(&state, &build_id, new_stage).await;

                        // Send stage change to WebSocket client if different from last sent
                        if last_sent_stage != Some(new_stage) {
                            if send_stage_change(&mut socket, new_stage).await {
                                last_sent_stage = Some(new_stage);
                            }
                        }
                    }

                    // Send the log line over WebSocket
                    if socket
                        .send(Message::Text(line_buf.clone().into()))
                        .await
                        .is_err()
                    {
                        // Client disconnected
                        tracing::info!("WebSocket client disconnected for build {}", build_id);
                        return;
                    }
                }
                Err(e) => {
                    let _ = socket
                        .send(Message::Text(
                            format!("[Error reading log: {}]", e).into(),
                        ))
                        .await;
                    break;
                }
            }
        }

        // Remember current position
        if let Ok(pos) = reader.stream_position().await {
            last_position = pos;
        }

        // Check if build is still running and get current stage
        let (build_status, current_stage) = {
            let state_read = state.read().await;
            let status = state_read
                .db
                .get_build(&build_id)
                .await
                .ok()
                .flatten()
                .map(|b| b.status);
            let stage = state_read.build_stages.get(&build_id).copied();
            (status, stage)
        };

        // Send stage update if it changed (from host-side operations like moving/indexing)
        if let Some(stage) = current_stage {
            if last_sent_stage != Some(stage) {
                if send_stage_change(&mut socket, stage).await {
                    last_sent_stage = Some(stage);
                }
            }
        }

        match build_status {
            Some(crate::models::BuildStatus::Running) | Some(crate::models::BuildStatus::Pending) => {
                // Build still in progress, wait and check for more output
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

                // Re-open file to check for new content (handles file being appended to)
                if let Ok(file) = tokio::fs::File::open(&log_path).await {
                    reader = tokio::io::BufReader::new(file);
                    if reader.seek(std::io::SeekFrom::Start(last_position)).await.is_err() {
                        continue;
                    }
                }
            }
            Some(status) => {
                // Build completed (success, failed, or cancelled)
                // Read any remaining content
                loop {
                    line_buf.clear();
                    match reader.read_line(&mut line_buf).await {
                        Ok(0) => break,
                        Ok(_) => {
                            // Check for stage markers in remaining lines too
                            if let Some(new_stage) = BuildStage::from_log_line(&line_buf) {
                                if last_sent_stage != Some(new_stage) {
                                    if send_stage_change(&mut socket, new_stage).await {
                                        last_sent_stage = Some(new_stage);
                                    }
                                }
                            }
                            let _ = socket.send(Message::Text(line_buf.clone().into())).await;
                        }
                        Err(_) => break,
                    }
                }

                // Send final stage
                let final_stage = match status {
                    crate::models::BuildStatus::Success => BuildStage::Complete,
                    _ => BuildStage::Failed,
                };
                if last_sent_stage != Some(final_stage) {
                    let _ = send_stage_change(&mut socket, final_stage).await;
                }

                // Send completion message
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
                // Build not found (deleted?)
                let _ = socket
                    .send(Message::Text("[Build not found]".to_string().into()))
                    .await;
                break;
            }
        }

        // Check for incoming messages (ping/pong, close)
        match tokio::time::timeout(
            tokio::time::Duration::from_millis(10),
            socket.recv(),
        )
        .await
        {
            Ok(Some(Ok(Message::Close(_)))) => {
                tracing::info!("WebSocket close requested for build {}", build_id);
                break;
            }
            Ok(Some(Ok(Message::Ping(data)))) => {
                let _ = socket.send(Message::Pong(data)).await;
            }
            Ok(Some(Err(_))) | Ok(None) => {
                // Connection error or closed
                break;
            }
            _ => {
                // Timeout or other message, continue
            }
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

/// Queue multiple builds in dependency order
async fn queue_builds(
    State(state): State<SharedState>,
    Json(req): Json<QueueBuildsRequest>,
) -> Result<Json<QueueBuildsResponse>, AppError> {
    if req.packages.is_empty() {
        return Err(AppError::bad_request("No packages specified"));
    }

    let state_read = state.read().await;

    // Check base image exists
    match state_read.build.image_exists() {
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
        match resolve_package(&state_read, name).await {
            Ok(pkg) => {
                if pkg.status != PackageStatus::Approved && pkg.status != PackageStatus::UpdateAvailable {
                    skipped.push(QueueSkipped {
                        pkgbase: pkg.pkgbase,
                        reason: format!("Package status is '{}', must be 'approved' or 'update_available'", pkg.status),
                    });
                } else if pkg.current_commit.is_none() {
                    skipped.push(QueueSkipped {
                        pkgbase: pkg.pkgbase,
                        reason: "Package has no current commit".to_string(),
                    });
                } else {
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
        return Ok(Json(QueueBuildsResponse {
            build_order: vec![],
            build_ids: vec![],
            skipped,
        }));
    }

    // Build dependency graph for topological sort
    // Map pkgbase -> set of pkgbases it depends on (that are also in our build list)
    let pkgbases_set: std::collections::HashSet<String> = packages_to_build
        .iter()
        .map(|p| p.pkgbase.clone())
        .collect();

    // Also map pkgnames to pkgbases for dependency lookup
    let mut pkgname_to_pkgbase: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for pkg in &packages_to_build {
        let names = state_read.db.get_package_names(&pkg.pkgbase).await?;
        for name in names {
            pkgname_to_pkgbase.insert(name.pkgname, pkg.pkgbase.clone());
        }
    }

    // Build the dependency graph: pkgbase -> Vec<pkgbases it depends on>
    let mut deps_graph: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    for pkg in &packages_to_build {
        let names = state_read.db.get_package_names(&pkg.pkgbase).await?;
        let mut pkg_deps: Vec<String> = Vec::new();

        for name in &names {
            if let Some(deps_json) = &name.depends {
                if let Ok(deps) = serde_json::from_str::<Vec<String>>(deps_json) {
                    for dep in deps {
                        let dep_name = crate::aur::DepResolver::dep_name(&dep);
                        // Check if this dependency is one of the packages we're building
                        if let Some(dep_pkgbase) = pkgname_to_pkgbase.get(dep_name) {
                            if dep_pkgbase != &pkg.pkgbase && pkgbases_set.contains(dep_pkgbase) {
                                pkg_deps.push(dep_pkgbase.clone());
                            }
                        }
                    }
                }
            }
        }

        pkg_deps.sort();
        pkg_deps.dedup();
        deps_graph.insert(pkg.pkgbase.clone(), pkg_deps);
    }

    // Topological sort using Kahn's algorithm
    let build_order = topological_sort(&pkgbases_set, &deps_graph)?;

    // Create build records for each package
    let config = state_read.config.clone();
    drop(state_read);

    let mut build_ids: Vec<String> = Vec::new();
    let mut pkg_info: Vec<(String, String, String, std::path::PathBuf)> = Vec::new(); // (pkgbase, build_id, local_path, log_path)

    {
        let state_write = state.write().await;
        let now = chrono::Utc::now();

        for pkgbase in &build_order {
            let pkg = packages_to_build.iter().find(|p| &p.pkgbase == pkgbase).unwrap();
            let build_id = uuid::Uuid::new_v4().to_string();
            let log_path = config.resolved_cache().logs.join(format!("{}.log", build_id));
            let commit = pkg.current_commit.clone().unwrap();

            let build = crate::models::Build {
                id: build_id.clone(),
                pkgbase: pkgbase.clone(),
                commit,
                status: crate::models::BuildStatus::Pending,
                started_at: now,
                finished_at: None,
                log_path: Some(log_path.to_string_lossy().to_string()),
                outputs: None,
                error_message: None,
            };

            state_write.db.insert_build(&build).await?;
            build_ids.push(build_id.clone());
            pkg_info.push((
                pkgbase.clone(),
                build_id,
                pkg.local_path.clone(),
                log_path,
            ));
        }
    }

    // Spawn background task to run builds sequentially
    let state_clone = state.clone();
    let deps_graph_clone = deps_graph.clone();

    tokio::spawn(async move {
        run_queued_builds(state_clone, pkg_info, deps_graph_clone, config).await;
    });

    Ok(Json(QueueBuildsResponse {
        build_order,
        build_ids,
        skipped,
    }))
}

/// Topological sort using Kahn's algorithm
fn topological_sort(
    nodes: &std::collections::HashSet<String>,
    deps: &std::collections::HashMap<String, Vec<String>>,
) -> Result<Vec<String>, AppError> {
    use std::collections::{HashMap, HashSet, VecDeque};

    // Calculate in-degree for each node
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    for node in nodes {
        in_degree.insert(node.clone(), 0);
    }

    for node in nodes {
        if let Some(node_deps) = deps.get(node) {
            for dep in node_deps {
                if nodes.contains(dep) {
                    *in_degree.entry(node.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    // Start with nodes that have no dependencies (in-degree 0)
    let mut queue: VecDeque<String> = VecDeque::new();
    for (node, &degree) in &in_degree {
        if degree == 0 {
            queue.push_back(node.clone());
        }
    }

    let mut result: Vec<String> = Vec::new();
    let mut processed: HashSet<String> = HashSet::new();

    while let Some(node) = queue.pop_front() {
        result.push(node.clone());
        processed.insert(node.clone());

        // For each node that depends on this one, decrease its in-degree
        for other in nodes {
            if let Some(other_deps) = deps.get(other) {
                if other_deps.contains(&node) && !processed.contains(other) {
                    let degree = in_degree.get_mut(other).unwrap();
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(other.clone());
                    }
                }
            }
        }
    }

    if result.len() != nodes.len() {
        return Err(AppError::bad_request(
            "Circular dependency detected in build queue",
        ));
    }

    Ok(result)
}

/// Background task that runs queued builds sequentially
async fn run_queued_builds(
    state: SharedState,
    builds: Vec<(String, String, String, std::path::PathBuf)>, // (pkgbase, build_id, local_path, log_path)
    deps_graph: std::collections::HashMap<String, Vec<String>>,
    config: crate::config::Config,
) {
    use std::collections::HashSet;

    let mut failed_pkgbases: HashSet<String> = HashSet::new();

    for (pkgbase, build_id, local_path, log_path) in builds {
        // Check if any of this package's dependencies failed
        let should_skip = if let Some(pkg_deps) = deps_graph.get(&pkgbase) {
            pkg_deps.iter().any(|dep| failed_pkgbases.contains(dep))
        } else {
            false
        };

        if should_skip {
            tracing::info!(
                "Skipping build {} for {} due to failed dependency",
                build_id,
                pkgbase
            );

            let state_write = state.write().await;
            let _ = state_write
                .db
                .update_build_status(
                    &build_id,
                    crate::models::BuildStatus::Cancelled,
                    Some("Skipped: dependency build failed"),
                )
                .await;
            drop(state_write);

            failed_pkgbases.insert(pkgbase);
            continue;
        }

        // Initialize stage tracking
        set_build_stage(&state, &build_id, BuildStage::Starting).await;

        // Update status to Building
        {
            let state_write = state.write().await;
            let _ = state_write
                .db
                .update_package_status(&pkgbase, PackageStatus::Building)
                .await;
            let _ = state_write
                .db
                .update_build_status(&build_id, crate::models::BuildStatus::Running, None)
                .await;
        }

        tracing::info!("Starting queued build {} for {}", build_id, pkgbase);

        // Run the build
        let source_path = std::path::PathBuf::from(&local_path);
        let log_path_clone = log_path.clone();
        let pkgbase_clone = pkgbase.clone();
        let config_clone = config.clone();

        let build_result = tokio::task::spawn_blocking(move || {
            let build_system = BuildSystem::new(config_clone);
            build_system.run_build_sync(&pkgbase_clone, &source_path, &log_path_clone)
        })
        .await;

        match build_result {
            Ok(Ok(packages)) => {
                // Build succeeded
                tracing::info!("Build {} succeeded for {}", build_id, pkgbase);

                set_build_stage(&state, &build_id, BuildStage::MovingToRepo).await;

                let build_system = BuildSystem::new(config.clone());

                match build_system.move_to_repo(&packages) {
                    Ok(repo_packages) => {
                        set_build_stage(&state, &build_id, BuildStage::UpdatingIndex).await;

                        if let Err(e) = build_system.repo_add(&repo_packages) {
                            tracing::error!("repo-add failed for {}: {}", pkgbase, e);
                            set_build_stage(&state, &build_id, BuildStage::Failed).await;
                            failed_pkgbases.insert(pkgbase.clone());
                            update_build_failed(
                                &state,
                                &build_id,
                                &pkgbase,
                                &format!("repo-add failed: {}", e),
                            )
                            .await;
                            clear_build_stage(&state, &build_id).await;
                            continue;
                        }

                        let outputs: Vec<String> = repo_packages
                            .iter()
                            .filter_map(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .collect();
                        let outputs_json = serde_json::to_string(&outputs).unwrap_or_default();

                        set_build_stage(&state, &build_id, BuildStage::Complete).await;

                        let state_write = state.write().await;
                        let _ = state_write
                            .db
                            .update_build_status(&build_id, crate::models::BuildStatus::Success, None)
                            .await;
                        let _ = state_write
                            .db
                            .update_build_outputs(&build_id, &outputs_json)
                            .await;
                        let _ = state_write
                            .db
                            .update_package_status(&pkgbase, PackageStatus::UpToDate)
                            .await;
                        drop(state_write);

                        clear_build_stage(&state, &build_id).await;
                    }
                    Err(e) => {
                        tracing::error!("Failed to move packages for {}: {}", pkgbase, e);
                        set_build_stage(&state, &build_id, BuildStage::Failed).await;
                        failed_pkgbases.insert(pkgbase.clone());
                        update_build_failed(
                            &state,
                            &build_id,
                            &pkgbase,
                            &format!("Failed to move packages: {}", e),
                        )
                        .await;
                        clear_build_stage(&state, &build_id).await;
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!("Build {} failed for {}: {}", build_id, pkgbase, e);
                set_build_stage(&state, &build_id, BuildStage::Failed).await;
                failed_pkgbases.insert(pkgbase.clone());
                update_build_failed(&state, &build_id, &pkgbase, &e.to_string()).await;
                clear_build_stage(&state, &build_id).await;
            }
            Err(e) => {
                tracing::error!("Build task panicked for {}: {}", pkgbase, e);
                set_build_stage(&state, &build_id, BuildStage::Failed).await;
                failed_pkgbases.insert(pkgbase.clone());
                update_build_failed(
                    &state,
                    &build_id,
                    &pkgbase,
                    &format!("Build task panicked: {}", e),
                )
                .await;
                clear_build_stage(&state, &build_id).await;
            }
        }
    }

    tracing::info!(
        "Build queue completed. {} failed builds.",
        failed_pkgbases.len()
    );
}

async fn cancel_build(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let state = state.write().await;

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

    // Only attempt to kill container for running builds
    if is_running {
        let container_name = format!("repoman-build-{}-{}", build.pkgbase, build.id);
        state.build.stop_build(&container_name)?;
    }

    state
        .db
        .update_build_status(&id, crate::models::BuildStatus::Cancelled, None)
        .await?;
    state
        .db
        .update_package_status(&build.pkgbase, PackageStatus::Approved)
        .await?;

    Ok(StatusCode::OK)
}

// ==================== System handlers ====================

async fn repo_info(
    State(state): State<SharedState>,
) -> Result<Json<crate::repo::RepoStats>, AppError> {
    let state = state.read().await;
    let stats = state.repo.stats()?;
    Ok(Json(stats))
}

async fn clean_repo(State(state): State<SharedState>) -> Result<Json<CleanResponse>, AppError> {
    let state = state.read().await;
    let deleted = state
        .repo
        .clean_old_versions(state.config.repo.keep_versions)?;
    Ok(Json(CleanResponse {
        deleted_count: deleted.len() as u32,
        deleted_files: deleted,
    }))
}

async fn rebuild_repo_db(State(state): State<SharedState>) -> Result<StatusCode, AppError> {
    let state = state.read().await;
    state.repo.rebuild_database()?;
    Ok(StatusCode::OK)
}

/// Remove a package from the repository files but keep it tracked.
/// The package status is set to Approved so it can be rebuilt.
async fn remove_from_repo(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<Json<RemoveFromRepoResponse>, AppError> {
    let state = state.write().await;

    // Resolve pkgname -> pkgbase if needed
    let pkg = resolve_package(&state, &name).await?;
    let pkg_names = state.db.get_package_names(&pkg.pkgbase).await?;

    // Remove from repo database and delete files
    let pkgnames: Vec<&str> = pkg_names.iter().map(|n| n.pkgname.as_str()).collect();
    let _ = state.build.repo_remove(&pkgnames); // Ignore errors if not in repo

    let mut deleted_files = Vec::new();
    for pkg_name in &pkg_names {
        if let Ok(deleted) = state.repo.delete_package_files(&pkg_name.pkgname) {
            deleted_files.extend(deleted);
        }
    }

    // Set status to Approved so it can be rebuilt
    state
        .db
        .update_package_status(&pkg.pkgbase, PackageStatus::Approved)
        .await?;

    Ok(Json(RemoveFromRepoResponse {
        pkgbase: pkg.pkgbase,
        deleted_files,
    }))
}

async fn system_status(State(state): State<SharedState>) -> Result<Json<SystemStatusResponse>, AppError> {
    let state = state.read().await;

    let podman = BuildSystem::check_podman().is_ok();
    let image = state.build.image_exists().unwrap_or(false);
    let repo = state.repo.stats().ok();

    let packages = state.db.list_packages().await?;

    // Collect tracked pkgbases for orphan detection
    let tracked_pkgbases: std::collections::HashSet<String> = packages
        .iter()
        .map(|p| p.pkgbase.clone())
        .collect();

    let mut counts = PackageCounts {
        total: packages.len() as u32,
        pending_review: 0,
        approved: 0,
        up_to_date: 0,
        update_available: 0,
        failed: 0,
    };

    for pkg in packages {
        match pkg.status {
            PackageStatus::PendingReview => counts.pending_review += 1,
            PackageStatus::Approved => counts.approved += 1,
            PackageStatus::UpToDate => counts.up_to_date += 1,
            PackageStatus::UpdateAvailable => counts.update_available += 1,
            PackageStatus::Failed => counts.failed += 1,
            _ => {}
        }
    }

    // Calculate cache stats
    let pacman_cache = calculate_pacman_cache_stats(&state.config.resolved_cache().pacman_cache);
    let source_cache = calculate_source_cache_stats(&state.config.resolved_cache().sources);

    // Detect orphaned data
    let known_log_files = state.db.list_build_log_filenames().await.unwrap_or_default();
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

/// Get a documented TOML config template
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
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz") {
                    package_count += 1;
                    if let Ok(meta) = entry.metadata() {
                        total_size += meta.len();
                    }
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
                // Sum up sizes of all files in this package's source dir
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
    let state = state.read().await;
    state.build.build_base_image()?;
    Ok(StatusCode::OK)
}

async fn clean_cache(State(state): State<SharedState>) -> Result<Json<CleanCacheResponse>, AppError> {
    let state = state.read().await;
    let cache_path = &state.config.resolved_cache().pacman_cache;

    // Keep last 2 versions of each package (like paccache -rk2)
    let keep_versions = 2usize;

    let result = clean_pacman_cache(cache_path, keep_versions);

    Ok(Json(result))
}

/// Parse a pacman package filename into (pkgname, version_string, full_path)
/// Filename format: {pkgname}-{pkgver}-{pkgrel}-{arch}.pkg.tar.{zst,xz}
fn parse_pacman_pkg_filename(path: &std::path::Path) -> Option<(String, String)> {
    let filename = path.file_name()?.to_str()?;

    let name = filename
        .strip_suffix(".pkg.tar.zst")
        .or_else(|| filename.strip_suffix(".pkg.tar.xz"))?;

    // Split from the end: pkgname-pkgver-pkgrel-arch
    // We need at least 4 parts
    let parts: Vec<&str> = name.rsplitn(4, '-').collect();
    if parts.len() < 4 {
        return None;
    }

    let arch = parts[0];
    let pkgrel = parts[1];
    let pkgver = parts[2];
    let pkgname = parts[3];

    // Version string for sorting (pkgver-pkgrel-arch)
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

    // Group packages by name
    if let Ok(entries) = std::fs::read_dir(cache_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some((pkgname, version)) = parse_pacman_pkg_filename(&path) {
                by_name
                    .entry(pkgname)
                    .or_default()
                    .push((path, version));
            }
        }
    }

    // For each package, keep only the newest N versions
    for (_name, mut versions) in by_name {
        // Sort by version (newest first) using vercmp
        versions.sort_by(|a, b| vercmp(&b.1, &a.1));

        // Delete versions beyond the keep limit
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

async fn clean_sources(State(state): State<SharedState>) -> Result<Json<CleanSourcesResponse>, AppError> {
    let state = state.read().await;
    let sources_path = &state.config.resolved_cache().sources;

    let mut cleaned_count = 0u32;
    let mut cleaned_size = 0u64;

    if sources_path.exists() {
        if let Ok(entries) = std::fs::read_dir(sources_path) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    // Calculate size before deleting
                    if let Ok(files) = std::fs::read_dir(entry.path()) {
                        for file in files.flatten() {
                            if let Ok(meta) = file.metadata() {
                                cleaned_size += meta.len();
                            }
                        }
                    }
                    // Use podman unshare to handle permission issues
                    let path_str = entry.path().to_string_lossy().to_string();
                    let status = std::process::Command::new("podman")
                        .args(["unshare", "rm", "-rf", &path_str])
                        .status();

                    match status {
                        Ok(s) if s.success() => cleaned_count += 1,
                        _ => {
                            // Fallback to regular rm
                            if std::fs::remove_dir_all(entry.path()).is_ok() {
                                cleaned_count += 1;
                            }
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
    tracked_pkgbases: &std::collections::HashSet<String>,
    known_log_files: &std::collections::HashSet<String>,
) -> Option<OrphanedData> {
    let mut orphaned = OrphanedData {
        clones: Vec::new(),
        sources: Vec::new(),
        pkgdests: Vec::new(),
        logs: Vec::new(),
        total_size: 0,
    };

    // Check AUR clones directory
    if let Ok(entries) = std::fs::read_dir(&config.resolved_cache().aur_clones) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if !tracked_pkgbases.contains(name) {
                        orphaned.total_size += dir_size(&entry.path());
                        orphaned.clones.push(name.to_string());
                    }
                }
            }
        }
    }

    // Check sources directory
    if let Ok(entries) = std::fs::read_dir(&config.resolved_cache().sources) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if !tracked_pkgbases.contains(name) {
                        orphaned.total_size += dir_size(&entry.path());
                        orphaned.sources.push(name.to_string());
                    }
                }
            }
        }
    }

    // Check logs directory for orphaned pkgdest-* dirs and build logs
    if let Ok(entries) = std::fs::read_dir(&config.resolved_cache().logs) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                // Check pkgdest directories
                if let Some(pkgbase) = name.strip_prefix("pkgdest-") {
                    if !tracked_pkgbases.contains(pkgbase) {
                        orphaned.total_size += dir_size(&entry.path());
                        orphaned.pkgdests.push(pkgbase.to_string());
                    }
                }
                // Check build logs ({uuid}.log)
                else if name.ends_with(".log") && !known_log_files.contains(name) {
                    if let Ok(meta) = entry.metadata() {
                        orphaned.total_size += meta.len();
                    }
                    orphaned.logs.push(name.to_string());
                }
            }
        }
    }

    // Return None if nothing orphaned
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

async fn clean_orphans(State(state): State<SharedState>) -> Result<Json<CleanOrphansResponse>, AppError> {
    let state = state.read().await;

    let packages = state.db.list_packages().await?;
    let tracked_pkgbases: std::collections::HashSet<String> = packages
        .iter()
        .map(|p| p.pkgbase.clone())
        .collect();

    let known_log_files = state.db.list_build_log_filenames().await.unwrap_or_default();
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

    // Clean orphaned clones
    for name in &orphaned.clones {
        let path = state.config.resolved_cache().aur_clones.join(name);
        if std::fs::remove_dir_all(&path).is_ok() {
            cleaned.clones_removed += 1;
        }
    }

    // Clean orphaned sources (may need podman unshare)
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

    // Clean orphaned pkgdest directories (may need podman unshare)
    for name in &orphaned.pkgdests {
        let path = state.config.resolved_cache().logs.join(format!("pkgdest-{}", name));
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

    // Clean orphaned logs
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
    let state = state.read().await;
    let containers = state.build.list_containers()?;
    Ok(Json(containers))
}

async fn prune_containers(
    State(state): State<SharedState>,
) -> Result<Json<crate::build::PruneResult>, AppError> {
    let state = state.read().await;
    let result = state.build.prune_containers()?;
    Ok(Json(result))
}

async fn list_images(
    State(state): State<SharedState>,
) -> Result<Json<Vec<crate::build::ImageInfo>>, AppError> {
    let state = state.read().await;
    let images = state.build.list_images()?;
    Ok(Json(images))
}

#[derive(Deserialize)]
pub struct PruneImagesQuery {
    /// Number of images to keep (default: 3)
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
    let state = state.read().await;
    let result = state.build.prune_images(query.keep)?;
    Ok(Json(result))
}
