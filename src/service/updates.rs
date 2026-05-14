use std::sync::Arc;

use tokio::sync::broadcast;

use crate::api::{AppError, ServerEvent};
use crate::aur::{AurClient, AurGit, read_srcinfo};
use crate::build::vercmp;
use crate::models::Package;
use crate::state::SharedState;

/// Response for check_updates
pub struct CheckUpdatesResult {
    pub checked: usize,
    pub updated: Vec<String>,
}

/// Inner check_updates logic, reusable for the combined check-and-build endpoint.
pub async fn check_updates_inner(state: &SharedState) -> Result<CheckUpdatesResult, AppError> {
    let packages = state.db.list_packages().await?;
    let events_tx = state.events.clone();
    let aur_clones = state.config.resolved_cache().aur_clones;

    let total = packages.len();
    let checked_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // --- Phase 1: batch RPC version pre-check ---
    // Determine which packages are currently "building" via derived status
    let building_pkgbases: std::collections::HashSet<String> = {
        let active = state.active_builds.read().await;
        active.clone()
    };

    let aur_client = AurClient::new();

    let needs_fetch: std::collections::HashSet<String> = {
        let query_names: Vec<&str> = packages
            .iter()
            .filter(|p| !building_pkgbases.contains(&p.pkgbase))
            .map(|p| p.pkgbase.as_str())
            .collect();

        let mut rpc_versions: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for chunk in query_names.chunks(200) {
            match aur_client.info(chunk).await {
                Ok(infos) => {
                    for info in infos {
                        rpc_versions.insert(info.package_base.clone(), info.version.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "AUR RPC batch query failed, falling back to git fetch: {}",
                        e
                    );
                    return check_updates_git_only(state, packages, events_tx, total).await;
                }
            }
        }

        let cache = state.srcinfo_cache.read().await;
        let mut needs = std::collections::HashSet::new();
        for pkg in &packages {
            if building_pkgbases.contains(&pkg.pkgbase) {
                continue;
            }

            // VCS packages always need git fetch
            let is_vcs = pkg.pkgbase.ends_with("-git")
                || pkg.pkgbase.ends_with("-svn")
                || pkg.pkgbase.ends_with("-hg")
                || pkg.pkgbase.ends_with("-bzr");

            if is_vcs {
                needs.insert(pkg.pkgbase.clone());
                continue;
            }

            let local_ver = cache.get(&pkg.pkgbase).map(|c| c.srcinfo.full_version());

            match (rpc_versions.get(&pkg.pkgbase), local_ver) {
                (Some(rpc_ver), Some(local_ver)) => {
                    if vercmp(rpc_ver, &local_ver) != std::cmp::Ordering::Equal {
                        needs.insert(pkg.pkgbase.clone());
                    }
                }
                (Some(_), None) => {
                    needs.insert(pkg.pkgbase.clone());
                }
                (None, _) => {
                    needs.insert(pkg.pkgbase.clone());
                }
            }
        }

        tracing::info!(
            "RPC pre-check: {}/{} packages need git fetch",
            needs.len(),
            query_names.len()
        );

        needs
    };

    // --- Phase 2: git fetch only packages that need it ---
    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let mut join_set = tokio::task::JoinSet::new();

    for pkg in packages {
        if building_pkgbases.contains(&pkg.pkgbase) {
            let n = checked_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            crate::api::emit_event(
                &events_tx,
                ServerEvent::CheckUpdatesProgress {
                    checked: n,
                    total,
                    current: pkg.pkgbase.clone(),
                },
            );
            continue;
        }

        if !needs_fetch.contains(&pkg.pkgbase) {
            let n = checked_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            crate::api::emit_event(
                &events_tx,
                ServerEvent::CheckUpdatesProgress {
                    checked: n,
                    total,
                    current: pkg.pkgbase.clone(),
                },
            );
            continue;
        }

        let semaphore = semaphore.clone();
        let state = state.clone();
        let events_tx = events_tx.clone();
        let checked_counter = checked_counter.clone();
        let pkgbase = pkg.pkgbase.clone();
        let clone_path = aur_clones.join(&pkg.pkgbase);
        let auto_approve = pkg.auto_approve;
        let approved_commit = pkg.approved_commit.clone();

        join_set.spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();

            let clone_path_fetch = clone_path.clone();
            let pkgbase_for_err = pkgbase.clone();
            let has_updates = tokio::task::spawn_blocking(move || {
                AurGit::ensure_cloned(&pkgbase_for_err, &clone_path_fetch)?;
                AurGit::has_updates(&clone_path_fetch)
            })
            .await
            .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
            .map_err(|e| {
                AppError::internal(format!("git fetch failed for {}: {}", pkgbase.clone(), e))
            })?;

            let result = if has_updates {
                let clone_path_reset = clone_path.clone();
                let pkgbase_for_err = pkgbase.clone();
                tokio::task::spawn_blocking(move || AurGit::reset_to_remote(&clone_path_reset))
                    .await
                    .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
                    .map_err(|e| {
                        AppError::internal(format!(
                            "git reset failed for {}: {}",
                            pkgbase_for_err, e
                        ))
                    })?;

                // Re-read head commit and srcinfo
                let clone_path_head = clone_path.clone();
                let pkgbase_for_err = pkgbase.clone();
                let (new_commit, srcinfo) = tokio::task::spawn_blocking(move || {
                    let commit = AurGit::head_commit(&clone_path_head)?;
                    let srcinfo = read_srcinfo(&clone_path_head)?;
                    Ok::<_, anyhow::Error>((commit, srcinfo))
                })
                .await
                .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
                .map_err(|e| {
                    AppError::internal(format!(
                        "head_commit/srcinfo failed for {}: {}",
                        pkgbase_for_err, e
                    ))
                })?;

                // Update SrcInfoCache
                {
                    let mut cache = state.srcinfo_cache.write().await;
                    cache.insert(pkgbase.clone(), srcinfo, new_commit.clone());
                }

                // Auto-approve if configured and this is an update
                // (approved_commit is Some and differs from new HEAD)
                if auto_approve && approved_commit.is_some() {
                    state
                        .db
                        .approve_package(&pkgbase, &new_commit, true)
                        .await
                        .map_err(|e| AppError::internal(format!("DB error: {}", e)))?;
                    crate::api::emit_status_changed(&state, &pkgbase).await;
                } else {
                    crate::api::emit_status_changed(&state, &pkgbase).await;
                }

                Some(pkgbase.clone())
            } else {
                None
            };

            let n = checked_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            crate::api::emit_event(
                &events_tx,
                ServerEvent::CheckUpdatesProgress {
                    checked: n,
                    total,
                    current: pkgbase,
                },
            );

            Ok::<Option<String>, AppError>(result)
        });
    }

    // Collect results
    let mut updated = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(Some(pkgbase))) => updated.push(pkgbase),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                tracing::warn!("check_updates task failed: {}", e.message);
            }
            Err(e) => {
                tracing::warn!("check_updates task panicked: {}", e);
            }
        }
    }

    crate::api::emit_event(
        &events_tx,
        ServerEvent::CheckUpdatesComplete {
            checked: total,
            updated: updated.clone(),
        },
    );

    Ok(CheckUpdatesResult {
        checked: total,
        updated,
    })
}

/// Fallback: check all packages via git fetch
async fn check_updates_git_only(
    state: &SharedState,
    packages: Vec<Package>,
    events_tx: broadcast::Sender<ServerEvent>,
    total: usize,
) -> Result<CheckUpdatesResult, AppError> {
    let aur_clones = state.config.resolved_cache().aur_clones;
    let building_pkgbases: std::collections::HashSet<String> = {
        let active = state.active_builds.read().await;
        active.clone()
    };

    let checked_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
    let mut join_set = tokio::task::JoinSet::new();

    for pkg in packages {
        if building_pkgbases.contains(&pkg.pkgbase) {
            let n = checked_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            crate::api::emit_event(
                &events_tx,
                ServerEvent::CheckUpdatesProgress {
                    checked: n,
                    total,
                    current: pkg.pkgbase.clone(),
                },
            );
            continue;
        }

        let semaphore = semaphore.clone();
        let state = state.clone();
        let events_tx = events_tx.clone();
        let checked_counter = checked_counter.clone();
        let pkgbase = pkg.pkgbase.clone();
        let clone_path = aur_clones.join(&pkg.pkgbase);
        let auto_approve = pkg.auto_approve;
        let approved_commit = pkg.approved_commit.clone();

        join_set.spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();

            let clone_path_fetch = clone_path.clone();
            let pkgbase_for_clone = pkgbase.clone();
            let has_updates = tokio::task::spawn_blocking(move || {
                AurGit::ensure_cloned(&pkgbase_for_clone, &clone_path_fetch)?;
                AurGit::has_updates(&clone_path_fetch)
            })
            .await
            .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
            .map_err(|e| {
                AppError::internal(format!("git fetch failed for {}: {}", pkgbase.clone(), e))
            })?;

            let result = if has_updates {
                let clone_path_reset = clone_path.clone();
                let pkgbase_for_err = pkgbase.clone();
                tokio::task::spawn_blocking(move || AurGit::reset_to_remote(&clone_path_reset))
                    .await
                    .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
                    .map_err(|e| {
                        AppError::internal(format!(
                            "git reset failed for {}: {}",
                            pkgbase_for_err, e
                        ))
                    })?;

                let clone_path_head = clone_path.clone();
                let pkgbase_for_err = pkgbase.clone();
                let (new_commit, srcinfo) = tokio::task::spawn_blocking(move || {
                    let commit = AurGit::head_commit(&clone_path_head)?;
                    let srcinfo = read_srcinfo(&clone_path_head)?;
                    Ok::<_, anyhow::Error>((commit, srcinfo))
                })
                .await
                .map_err(|e| AppError::internal(format!("Task join error: {}", e)))?
                .map_err(|e| {
                    AppError::internal(format!(
                        "head_commit/srcinfo failed for {}: {}",
                        pkgbase_for_err, e
                    ))
                })?;

                // Update SrcInfoCache
                {
                    let mut cache = state.srcinfo_cache.write().await;
                    cache.insert(pkgbase.clone(), srcinfo, new_commit.clone());
                }

                // Auto-approve if configured
                if auto_approve && approved_commit.is_some() {
                    state
                        .db
                        .approve_package(&pkgbase, &new_commit, true)
                        .await
                        .map_err(|e| AppError::internal(format!("DB error: {}", e)))?;
                    crate::api::emit_status_changed(&state, &pkgbase).await;
                } else {
                    crate::api::emit_status_changed(&state, &pkgbase).await;
                }

                Some(pkgbase.clone())
            } else {
                None
            };

            let n = checked_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            crate::api::emit_event(
                &events_tx,
                ServerEvent::CheckUpdatesProgress {
                    checked: n,
                    total,
                    current: pkgbase,
                },
            );

            Ok::<Option<String>, AppError>(result)
        });
    }

    let mut updated = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(Some(pkgbase))) => updated.push(pkgbase),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                tracing::warn!("check_updates task failed: {}", e.message);
            }
            Err(e) => {
                tracing::warn!("check_updates task panicked: {}", e);
            }
        }
    }

    crate::api::emit_event(
        &events_tx,
        ServerEvent::CheckUpdatesComplete {
            checked: total,
            updated: updated.clone(),
        },
    );

    Ok(CheckUpdatesResult {
        checked: total,
        updated,
    })
}
