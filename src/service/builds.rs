use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::api::{AppError, ServerEvent};
use crate::aur::AurGit;
use crate::build::BuildSystem;
use crate::models::{BuildStage, BuildStatus};
use crate::state::SharedState;

/// Emit a server event on the broadcast channel. Ignores send errors (no receivers).
pub fn emit_event(tx: &tokio::sync::broadcast::Sender<ServerEvent>, event: ServerEvent) {
    let _ = tx.send(event);
}

/// Helper to update build stage in memory and emit SSE event.
pub async fn set_build_stage(
    state: &SharedState,
    build_id: &str,
    pkgbase: &str,
    stage: BuildStage,
) {
    let stage_str = stage.to_string();
    {
        let mut stages = state.build_stages.write().await;
        stages.insert(build_id.to_string(), stage);
    }
    emit_event(
        &state.events,
        ServerEvent::BuildStageChanged {
            build_id: build_id.to_string(),
            pkgbase: pkgbase.to_string(),
            stage: stage_str,
        },
    );
}

/// Helper to remove build stage from memory (when build completes).
pub async fn clear_build_stage(state: &SharedState, build_id: &str) {
    let mut stages = state.build_stages.write().await;
    stages.remove(build_id);
}

/// Helper to remove a pkgbase from active_builds and its cancel flag(s).
pub async fn clear_active_build(state: &SharedState, pkgbase: &str, build_id: &str) {
    {
        let mut active = state.active_builds.write().await;
        active.remove(pkgbase);
    }
    {
        let mut flags = state.build_cancel_flags.write().await;
        flags.remove(build_id);
    }
}

/// Helper to update build and package status on failure.
pub async fn update_build_failed(
    state: &SharedState,
    build_id: &str,
    pkgbase: &str,
    error: &str,
) {
    tracing::error!("Build {} failed: {}", build_id, error);
    if let Err(e) = state
        .db
        .update_build_status(build_id, BuildStatus::Failed, Some(error))
        .await
    {
        tracing::error!(
            "Failed to update build {} status to Failed: {}",
            build_id,
            e
        );
    }
    // V2: No update_package_status -- status is derived
    emit_event(
        &state.events,
        ServerEvent::BuildCompleted {
            build_id: build_id.to_string(),
            pkgbase: pkgbase.to_string(),
            status: "failed".to_string(),
        },
    );
    emit_event(
        &state.events,
        ServerEvent::PackageStatusChanged {
            pkgbase: pkgbase.to_string(),
            new_status: "failed".to_string(),
        },
    );
}

/// Background task that executes a single build.
pub async fn run_build_task(
    state: SharedState,
    pkgbase: String,
    build_id: String,
    log_path: std::path::PathBuf,
    config: crate::config::Config,
) {
    tracing::info!("Starting build task for {} (build {})", pkgbase, build_id);

    // Initialize stage tracking
    set_build_stage(&state, &build_id, &pkgbase, BuildStage::Starting).await;

    // Update status to Running
    if let Err(e) = state
        .db
        .update_build_status(&build_id, BuildStatus::Running, None)
        .await
    {
        tracing::error!("Failed to update build status to Running: {}", e);
        clear_build_stage(&state, &build_id).await;
        clear_active_build(&state, &pkgbase, &build_id).await;
        return;
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
        clear_active_build(&state, &pkgbase, &build_id).await;
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
        clear_active_build(&state, &pkgbase, &build_id).await;
        return;
    }

    // Ensure clone exists (re-clone from AUR if missing)
    let source_path = config.resolved_cache().aur_clones.join(&pkgbase);
    {
        let sp = source_path.clone();
        let pb = pkgbase.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || AurGit::ensure_cloned(&pb, &sp))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("{}", e)))
        {
            update_build_failed(
                &state,
                &build_id,
                &pkgbase,
                &format!("Failed to ensure clone exists: {}", e),
            )
            .await;
            clear_active_build(&state, &pkgbase, &build_id).await;
            return;
        }
    }

    // Run the build using spawn_blocking for the sync version
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

            set_build_stage(&state, &build_id, &pkgbase, BuildStage::MovingToRepo).await;

            let build_system = BuildSystem::new(config.clone());

            match build_system.move_to_repo(&packages) {
                Ok(repo_packages) => {
                    // Add to repo database under repo mutex
                    set_build_stage(&state, &build_id, &pkgbase, BuildStage::UpdatingIndex).await;

                    let repo_add_result = {
                        let _repo_lock = state.repo_mutex.lock().await;
                        build_system.repo_add(&repo_packages)
                    };

                    if let Err(e) = repo_add_result {
                        tracing::error!("repo-add failed: {}", e);
                        set_build_stage(&state, &build_id, &pkgbase, BuildStage::Failed).await;
                        update_build_failed(
                            &state,
                            &build_id,
                            &pkgbase,
                            &format!("repo-add failed: {}", e),
                        )
                        .await;
                        clear_build_stage(&state, &build_id).await;
                        clear_active_build(&state, &pkgbase, &build_id).await;
                        return;
                    }

                    // Update build record with outputs
                    let outputs: Vec<String> = repo_packages
                        .iter()
                        .filter_map(|p| p.file_name())
                        .map(|n| n.to_string_lossy().to_string())
                        .collect();
                    let outputs_json = serde_json::to_string(&outputs).unwrap_or_else(|_| "[]".to_string());

                    set_build_stage(&state, &build_id, &pkgbase, BuildStage::Complete).await;

                    // Check if the build was cancelled while we were working
                    let already_cancelled = state
                        .db
                        .get_build(&build_id)
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|b| b.status == BuildStatus::Cancelled);

                    if !already_cancelled {
                        if let Err(e) = state
                            .db
                            .update_build_status(&build_id, BuildStatus::Success, None)
                            .await
                        {
                            tracing::error!(
                                "Failed to update build {} status to Success: {}",
                                build_id,
                                e
                            );
                        }
                        if let Err(e) = state.db.update_build_outputs(&build_id, &outputs_json).await
                        {
                            tracing::error!("Failed to update build {} outputs: {}", build_id, e);
                        }
                        // V2: No update_package_status -- status is derived

                        // Reload RepoState after successful repo-add
                        {
                            let mut repo = state.repo_state.write().await;
                            if let Err(e) = repo.reload() {
                                tracing::error!("Failed to reload repo state: {}", e);
                            }
                        }

                        emit_event(
                            &state.events,
                            ServerEvent::BuildCompleted {
                                build_id: build_id.clone(),
                                pkgbase: pkgbase.clone(),
                                status: "success".to_string(),
                            },
                        );
                        emit_event(
                            &state.events,
                            ServerEvent::PackageStatusChanged {
                                pkgbase: pkgbase.clone(),
                                new_status: "up_to_date".to_string(),
                            },
                        );
                    }

                    clear_build_stage(&state, &build_id).await;
                    clear_active_build(&state, &pkgbase, &build_id).await;
                    tracing::info!("Build {} completed successfully", build_id);
                }
                Err(e) => {
                    set_build_stage(&state, &build_id, &pkgbase, BuildStage::Failed).await;
                    update_build_failed(
                        &state,
                        &build_id,
                        &pkgbase,
                        &format!("Failed to move packages: {}", e),
                    )
                    .await;
                    clear_build_stage(&state, &build_id).await;
                    clear_active_build(&state, &pkgbase, &build_id).await;
                }
            }
        }
        Ok(Err(e)) => {
            // Build command failed
            tracing::error!("Build failed for {}: {}", pkgbase, e);
            set_build_stage(&state, &build_id, &pkgbase, BuildStage::Failed).await;
            update_build_failed(&state, &build_id, &pkgbase, &e.to_string()).await;
            clear_build_stage(&state, &build_id).await;
            clear_active_build(&state, &pkgbase, &build_id).await;
        }
        Err(e) => {
            // Task panicked
            tracing::error!("Build task panicked for {}: {}", pkgbase, e);
            set_build_stage(&state, &build_id, &pkgbase, BuildStage::Failed).await;
            update_build_failed(
                &state,
                &build_id,
                &pkgbase,
                &format!("Build task panicked: {}", e),
            )
            .await;
            clear_build_stage(&state, &build_id).await;
            clear_active_build(&state, &pkgbase, &build_id).await;
        }
    }
}

/// Topological sort using Kahn's algorithm.
pub fn topological_sort(
    nodes: &HashSet<String>,
    deps: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>, AppError> {
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

    // Start with nodes that have no dependencies
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

/// Background task that runs queued builds sequentially.
pub async fn run_queued_builds(
    state: SharedState,
    builds: Vec<(String, String, std::path::PathBuf)>, // (pkgbase, build_id, log_path)
    deps_graph: HashMap<String, Vec<String>>,
    config: crate::config::Config,
    cancel_flag: Arc<AtomicBool>,
) {
    let mut failed_pkgbases: HashSet<String> = HashSet::new();

    for (pkgbase, build_id, log_path) in &builds {
        // Check if the queue has been cancelled
        if cancel_flag.load(Ordering::Relaxed) {
            tracing::info!(
                "Cancelling queued build {} for {} (queue cancelled)",
                build_id,
                pkgbase
            );

            let _ = state
                .db
                .update_build_status(
                    build_id,
                    BuildStatus::Cancelled,
                    Some("Queue cancelled"),
                )
                .await;
            // V2: No update_package_status
            emit_event(
                &state.events,
                ServerEvent::BuildCompleted {
                    build_id: build_id.clone(),
                    pkgbase: pkgbase.clone(),
                    status: "cancelled".to_string(),
                },
            );
            clear_active_build(&state, pkgbase, build_id).await;
            continue;
        }

        // Check if any of this package's dependencies failed
        let should_skip = if let Some(pkg_deps) = deps_graph.get(pkgbase) {
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

            let _ = state
                .db
                .update_build_status(
                    build_id,
                    BuildStatus::Cancelled,
                    Some("Skipped: dependency build failed"),
                )
                .await;
            emit_event(
                &state.events,
                ServerEvent::BuildCompleted {
                    build_id: build_id.clone(),
                    pkgbase: pkgbase.clone(),
                    status: "skipped".to_string(),
                },
            );
            failed_pkgbases.insert(pkgbase.clone());
            clear_active_build(&state, pkgbase, build_id).await;
            continue;
        }

        // Check if already building
        {
            let active = state.active_builds.read().await;
            if active.contains(pkgbase) {
                tracing::info!(
                    "Skipping build {} for {} (already building)",
                    build_id,
                    pkgbase
                );
                let _ = state
                    .db
                    .update_build_status(
                        build_id,
                        BuildStatus::Cancelled,
                        Some("Skipped: package already building"),
                    )
                    .await;
                emit_event(
                    &state.events,
                    ServerEvent::BuildCompleted {
                        build_id: build_id.clone(),
                        pkgbase: pkgbase.clone(),
                        status: "skipped".to_string(),
                    },
                );
                clear_active_build(&state, pkgbase, build_id).await;
                continue;
            }
        }

        // Mark as actively building
        {
            let mut active = state.active_builds.write().await;
            active.insert(pkgbase.clone());
        }

        // Initialize stage tracking
        set_build_stage(&state, build_id, pkgbase, BuildStage::Starting).await;

        // Update status to Running
        let _ = state
            .db
            .update_build_status(build_id, BuildStatus::Running, None)
            .await;
        // V2: No update_package_status
        emit_event(
            &state.events,
            ServerEvent::BuildStarted {
                build_id: build_id.clone(),
                pkgbase: pkgbase.clone(),
            },
        );
        emit_event(
            &state.events,
            ServerEvent::PackageStatusChanged {
                pkgbase: pkgbase.clone(),
                new_status: "building".to_string(),
            },
        );

        tracing::info!("Starting queued build {} for {}", build_id, pkgbase);

        // Ensure clone exists
        let source_path = config.resolved_cache().aur_clones.join(pkgbase);
        {
            let sp = source_path.clone();
            let pb = pkgbase.clone();
            let ensure_result =
                tokio::task::spawn_blocking(move || AurGit::ensure_cloned(&pb, &sp)).await;
            let clone_err = match ensure_result {
                Ok(Ok(_)) => None,
                Ok(Err(e)) => Some(format!("{}", e)),
                Err(e) => Some(format!("task join error: {}", e)),
            };
            if let Some(err_msg) = clone_err {
                tracing::error!("Failed to ensure clone for {}: {}", pkgbase, err_msg);
                let _ = state
                    .db
                    .update_build_status(
                        build_id,
                        BuildStatus::Failed,
                        Some(&format!(
                            "Clone missing and re-clone failed: {}",
                            err_msg
                        )),
                    )
                    .await;
                // V2: No update_package_status
                failed_pkgbases.insert(pkgbase.clone());
                continue;
            }
        }

        // Run the build
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
                tracing::info!("Build {} succeeded for {}", build_id, pkgbase);

                set_build_stage(&state, build_id, pkgbase, BuildStage::MovingToRepo).await;

                let build_system = BuildSystem::new(config.clone());

                match build_system.move_to_repo(&packages) {
                    Ok(repo_packages) => {
                        set_build_stage(&state, build_id, pkgbase, BuildStage::UpdatingIndex).await;

                        let repo_add_result = {
                            let _repo_lock = state.repo_mutex.lock().await;
                            build_system.repo_add(&repo_packages)
                        };

                        if let Err(e) = repo_add_result {
                            tracing::error!("repo-add failed for {}: {}", pkgbase, e);
                            set_build_stage(&state, build_id, pkgbase, BuildStage::Failed).await;
                            failed_pkgbases.insert(pkgbase.clone());
                            update_build_failed(
                                &state,
                                build_id,
                                pkgbase,
                                &format!("repo-add failed: {}", e),
                            )
                            .await;
                            clear_build_stage(&state, build_id).await;
                            clear_active_build(&state, pkgbase, build_id).await;
                            continue;
                        }

                        let outputs: Vec<String> = repo_packages
                            .iter()
                            .filter_map(|p| p.file_name())
                            .map(|n| n.to_string_lossy().to_string())
                            .collect();
                        let outputs_json =
                            serde_json::to_string(&outputs).unwrap_or_default();

                        set_build_stage(&state, build_id, pkgbase, BuildStage::Complete).await;

                        let already_cancelled = state
                            .db
                            .get_build(build_id)
                            .await
                            .ok()
                            .flatten()
                            .is_some_and(|b| b.status == BuildStatus::Cancelled);

                        if !already_cancelled {
                            let _ = state
                                .db
                                .update_build_status(build_id, BuildStatus::Success, None)
                                .await;
                            let _ = state
                                .db
                                .update_build_outputs(build_id, &outputs_json)
                                .await;
                            // V2: No update_package_status

                            // Reload RepoState
                            {
                                let mut repo = state.repo_state.write().await;
                                if let Err(e) = repo.reload() {
                                    tracing::error!("Failed to reload repo state: {}", e);
                                }
                            }

                            emit_event(
                                &state.events,
                                ServerEvent::BuildCompleted {
                                    build_id: build_id.clone(),
                                    pkgbase: pkgbase.clone(),
                                    status: "success".to_string(),
                                },
                            );
                            emit_event(
                                &state.events,
                                ServerEvent::PackageStatusChanged {
                                    pkgbase: pkgbase.clone(),
                                    new_status: "up_to_date".to_string(),
                                },
                            );
                        }

                        clear_build_stage(&state, build_id).await;
                        clear_active_build(&state, pkgbase, build_id).await;
                    }
                    Err(e) => {
                        tracing::error!("Failed to move packages for {}: {}", pkgbase, e);
                        set_build_stage(&state, build_id, pkgbase, BuildStage::Failed).await;
                        failed_pkgbases.insert(pkgbase.clone());
                        update_build_failed(
                            &state,
                            build_id,
                            pkgbase,
                            &format!("Failed to move packages: {}", e),
                        )
                        .await;
                        clear_build_stage(&state, build_id).await;
                        clear_active_build(&state, pkgbase, build_id).await;
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!("Build {} failed for {}: {}", build_id, pkgbase, e);
                set_build_stage(&state, build_id, pkgbase, BuildStage::Failed).await;
                failed_pkgbases.insert(pkgbase.clone());
                update_build_failed(&state, build_id, pkgbase, &e.to_string()).await;
                clear_build_stage(&state, build_id).await;
                clear_active_build(&state, pkgbase, build_id).await;
            }
            Err(e) => {
                tracing::error!("Build task panicked for {}: {}", pkgbase, e);
                set_build_stage(&state, build_id, pkgbase, BuildStage::Failed).await;
                failed_pkgbases.insert(pkgbase.clone());
                update_build_failed(
                    &state,
                    build_id,
                    pkgbase,
                    &format!("Build task panicked: {}", e),
                )
                .await;
                clear_build_stage(&state, build_id).await;
                clear_active_build(&state, pkgbase, build_id).await;
            }
        }
    }

    // Clean up cancel flags for all builds in the queue
    {
        let mut flags = state.build_cancel_flags.write().await;
        for (_, build_id, _) in &builds {
            flags.remove(build_id);
        }
    }

    tracing::info!(
        "Build queue completed. {} failed builds.",
        failed_pkgbases.len()
    );
}
