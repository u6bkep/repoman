use std::collections::HashSet;

use crate::api::AppError;
use crate::aur::{AurClient, AurGit, DepResolver, MissingDep, read_srcinfo};
use crate::models::{InstallReason, Package};
use crate::state::AppState;

/// Look up pkgname -> pkgbase mapping using SrcInfoCache.
/// Returns Some(pkgbase) if a cached SRCINFO contains this pkgname.
pub async fn resolve_pkgbase(state: &AppState, name: &str) -> Option<String> {
    let cache = state.srcinfo_cache.read().await;
    for (pkgbase, cached) in cache.iter() {
        for pkg in &cached.srcinfo.packages {
            if pkg.pkgname == name {
                return Some(pkgbase.to_string());
            }
        }
    }
    None
}

/// Resolve a name (pkgbase or pkgname) to a Package.
pub async fn resolve_package(state: &AppState, name: &str) -> Result<Package, AppError> {
    // Try by pkgbase first
    if let Some(pkg) = state.db.get_package(name).await? {
        return Ok(pkg);
    }

    // Try by pkgname via srcinfo cache
    if let Some(pkgbase) = resolve_pkgbase(state, name).await {
        if let Some(pkg) = state.db.get_package(&pkgbase).await? {
            return Ok(pkg);
        }
    }

    Err(AppError::not_found("Package not found"))
}

/// Get all tracked package names (pkgbases + pkgnames from srcinfo cache).
pub async fn get_tracked_pkgnames(state: &AppState) -> Result<HashSet<String>, AppError> {
    let tracked_packages = state.db.list_packages().await?;
    let cache = state.srcinfo_cache.read().await;
    let mut tracked_pkgnames: HashSet<String> = HashSet::new();

    for p in &tracked_packages {
        tracked_pkgnames.insert(p.pkgbase.clone());
        if let Some(cached) = cache.get(&p.pkgbase) {
            for pkg in &cached.srcinfo.packages {
                tracked_pkgnames.insert(pkg.pkgname.clone());
            }
        }
    }
    Ok(tracked_pkgnames)
}

/// Result from adding a package.
pub struct AddPackageResult {
    pub pkg: Package,
    pub version: String,
}

/// Add a single package: does AUR lookup, git clone, SRCINFO parsing,
/// then inserts into DB and updates srcinfo cache.
pub async fn add_package_single(
    state: &AppState,
    name: &str,
    install_reason: InstallReason,
) -> Result<AddPackageResult, AppError> {
    // Check if already exists
    if let Some(pkg) = state.db.get_package(name).await? {
        let cache = state.srcinfo_cache.read().await;
        let version = cache
            .get(&pkg.pkgbase)
            .map(|c| c.srcinfo.full_version())
            .unwrap_or_default();
        return Ok(AddPackageResult { pkg, version });
    }

    // Try by pkgname
    if let Some(pkgbase) = resolve_pkgbase(state, name).await {
        if let Some(pkg) = state.db.get_package(&pkgbase).await? {
            let cache = state.srcinfo_cache.read().await;
            let version = cache
                .get(&pkg.pkgbase)
                .map(|c| c.srcinfo.full_version())
                .unwrap_or_default();
            return Ok(AddPackageResult { pkg, version });
        }
    }

    // Query AUR
    let aur_client = AurClient::new();
    let aur_info = aur_client
        .info_single(name)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Package '{}' not found in AUR", name)))?;

    let pkgbase = aur_info.package_base.clone();

    // Check again after AUR lookup (pkgbase may differ from name)
    if let Some(pkg) = state.db.get_package(&pkgbase).await? {
        let cache = state.srcinfo_cache.read().await;
        let version = cache
            .get(&pkg.pkgbase)
            .map(|c| c.srcinfo.full_version())
            .unwrap_or_default();
        return Ok(AddPackageResult { pkg, version });
    }

    // Clone repository
    let clone_path = state.config.resolved_cache().aur_clones.join(&pkgbase);
    if !clone_path.exists() {
        let pb = pkgbase.clone();
        let cp = clone_path.clone();
        tokio::task::spawn_blocking(move || AurGit::clone(&pb, &cp))
            .await
            .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
            .map_err(|e| AppError::internal(format!("Git clone failed: {}", e)))?;
    }

    // Parse .SRCINFO
    let clone_path_read = clone_path.clone();
    let (srcinfo, commit) = tokio::task::spawn_blocking(move || {
        let srcinfo = read_srcinfo(&clone_path_read)?;
        let commit = AurGit::head_commit(&clone_path_read)?;
        Ok::<_, anyhow::Error>((srcinfo, commit))
    })
    .await
    .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
    .map_err(|e| AppError::internal(format!("SRCINFO parse failed: {}", e)))?;

    let version = srcinfo.full_version();

    // Create package record (V2: no status, no current_commit, no pkgver in DB)
    let now = chrono::Utc::now();
    let pkg = Package {
        pkgbase: pkgbase.clone(),
        install_reason,
        auto_approve: false,
        approved_commit: None,
        added_at: now,
        updated_at: now,
    };

    // Double-check and insert
    if let Some(existing) = state.db.get_package(&pkgbase).await? {
        let cache = state.srcinfo_cache.read().await;
        let version = cache
            .get(&existing.pkgbase)
            .map(|c| c.srcinfo.full_version())
            .unwrap_or_default();
        return Ok(AddPackageResult {
            pkg: existing,
            version,
        });
    }

    state.db.insert_package(&pkg).await?;

    // Add to SrcInfoCache
    {
        let mut cache = state.srcinfo_cache.write().await;
        cache.insert(pkgbase.clone(), srcinfo, commit);
    }

    // Emit event
    crate::api::emit_event(
        &state.events,
        crate::api::ServerEvent::PackageStatusChanged {
            pkgbase: pkgbase.clone(),
            new_status: "pending_review".to_string(),
        },
    );

    Ok(AddPackageResult { pkg, version })
}

/// Find missing AUR dependencies for a package.
pub async fn find_missing_deps(
    state: &AppState,
    pkgbase: &str,
) -> Result<Vec<MissingDep>, AppError> {
    let clone_path = state.config.resolved_cache().aur_clones.join(pkgbase);
    let clone_path_read = clone_path.clone();
    let srcinfo = tokio::task::spawn_blocking(move || read_srcinfo(&clone_path_read))
        .await
        .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
        .map_err(|e| AppError::internal(format!("SRCINFO read failed: {}", e)))?;

    let tracked_pkgnames = get_tracked_pkgnames(state).await?;
    let container_image = state.config.container.image.clone();
    let repo_name = state.config.repo.name.clone();
    let repo_path = state.config.repo.path.clone();

    let aur_client = AurClient::new();
    let missing = aur_client
        .find_missing_aur_deps(
            &srcinfo,
            &tracked_pkgnames,
            &container_image,
            &repo_name,
            &repo_path,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to check for missing deps: {}", e);
            vec![]
        });

    Ok(missing)
}

/// Compute orphan list using SrcInfoCache.
pub async fn list_orphans(state: &AppState) -> Result<Vec<Package>, AppError> {
    let packages = state.db.list_packages().await?;
    let cache = state.srcinfo_cache.read().await;

    // Collect all dep names of explicitly installed packages
    let mut explicit_deps: HashSet<String> = HashSet::new();
    for pkg in &packages {
        if pkg.install_reason == InstallReason::Explicit {
            if let Some(cached) = cache.get(&pkg.pkgbase) {
                // Global deps
                for dep in &cached.srcinfo.depends {
                    explicit_deps.insert(DepResolver::dep_name(dep).to_string());
                }
                for dep in &cached.srcinfo.makedepends {
                    explicit_deps.insert(DepResolver::dep_name(dep).to_string());
                }
                for dep in &cached.srcinfo.checkdepends {
                    explicit_deps.insert(DepResolver::dep_name(dep).to_string());
                }
                // Per-package deps
                for sp in &cached.srcinfo.packages {
                    for dep in &sp.depends {
                        explicit_deps.insert(DepResolver::dep_name(dep).to_string());
                    }
                }
            }
        }
    }

    // Find dependency packages not depended on by explicit ones
    let mut orphans = Vec::new();
    for pkg in &packages {
        if pkg.install_reason != InstallReason::Dependency {
            continue;
        }
        if let Some(cached) = cache.get(&pkg.pkgbase) {
            let pkgnames: Vec<String> = cached
                .srcinfo
                .packages
                .iter()
                .map(|p| p.pkgname.clone())
                .collect();
            if crate::service::status::is_orphan(&pkgnames, &explicit_deps) {
                orphans.push(pkg.clone());
            }
        } else {
            // No srcinfo means no pkgnames, so it's an orphan
            orphans.push(pkg.clone());
        }
    }

    Ok(orphans)
}

/// Strip version constraint from dependency string.
pub fn parse_dep(dep: &str) -> (String, Option<String>) {
    for op in &[">=", "<=", "=", ">", "<"] {
        if let Some(pos) = dep.find(op) {
            return (dep[..pos].to_string(), Some(dep[pos..].to_string()));
        }
    }
    (dep.to_string(), None)
}
