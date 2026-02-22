mod api;
mod assets;
mod aur;
mod build;
mod config;
mod db;
mod models;
mod repo;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::api::{AppState, SharedState};
use crate::aur::AurClient;
use crate::build::BuildSystem;
use crate::config::Config;
use crate::db::Database;
use crate::repo::RepoManager;

#[derive(Parser)]
#[command(name = "repoman")]
#[command(about = "AUR package repository manager")]
struct Cli {
    /// Path to config file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Output in JSON format (machine-readable)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the web server
    Serve,

    /// Add a package to track
    Add {
        /// Package name (pkgname or pkgbase)
        name: String,
        /// Add as a dependency (not explicitly installed)
        #[arg(long)]
        asdeps: bool,
        /// Also add missing AUR dependencies
        #[arg(long)]
        with_deps: bool,
    },

    /// Remove a package
    Remove {
        /// Package name
        name: String,
        /// Also delete package files from repo
        #[arg(long)]
        delete_files: bool,
        /// Also delete git clone
        #[arg(long)]
        delete_clone: bool,
        /// Also remove orphaned dependencies (like pacman -Rs)
        #[arg(short = 's', long)]
        recursive: bool,
    },

    /// List tracked packages
    List {
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
        /// Show only explicitly installed packages
        #[arg(long, conflicts_with_all = ["deps", "orphans"])]
        explicit: bool,
        /// Show only packages installed as dependencies
        #[arg(long, conflicts_with_all = ["explicit", "orphans"])]
        deps: bool,
        /// Show only orphaned dependencies
        #[arg(long, conflicts_with_all = ["explicit", "deps"])]
        orphans: bool,
    },

    /// Change install reason for a package
    Mark {
        /// Package name
        name: String,
        /// Mark as explicitly installed
        #[arg(long, conflicts_with = "asdeps")]
        explicit: bool,
        /// Mark as dependency
        #[arg(long, conflicts_with = "explicit")]
        asdeps: bool,
    },

    /// Show package details
    Info {
        /// Package name
        name: String,
    },

    /// Show PKGBUILD diff
    Diff {
        /// Package name
        name: String,
    },

    /// Review PKGBUILD (opens in $EDITOR or prints to stdout)
    Review {
        /// Package name
        name: String,
        /// Print to stdout instead of opening editor
        #[arg(long)]
        stdout: bool,
    },

    /// Approve a package for building
    Approve {
        /// Package name
        name: String,
    },

    /// Build a package
    Build {
        /// Package name (or multiple names for queue)
        #[arg(required = true)]
        names: Vec<String>,
        /// Build all approved packages
        #[arg(long, conflicts_with = "updates")]
        all: bool,
        /// Build packages with updates available
        #[arg(long, conflicts_with = "all")]
        updates: bool,
    },

    /// View build logs
    Logs {
        /// Build ID or package name (for latest build)
        id: String,
        /// Follow log output (like tail -f)
        #[arg(short, long)]
        follow: bool,
        /// Number of lines to show from the end (default: all)
        #[arg(short = 'n', long)]
        lines: Option<usize>,
    },

    /// Check for updates
    CheckUpdates,

    /// Rebuild the base container image
    RebuildImage,

    /// Show system status
    Status,

    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Import packages from an existing pacman repository
    Import {
        /// Path to repository database (e.g., /srv/repo/aur/aur.db.tar.zst)
        db_path: PathBuf,
        /// Dry run - show what would be imported without making changes
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Clone)]
enum ConfigAction {
    /// Show current effective configuration (with defaults filled in)
    Show,
    /// Generate a configuration template with all options documented
    Template,
    /// Show path to config file
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "repoman=info,tower_http=debug".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    // Load config
    let config = Config::load(cli.config.as_deref())?;

    // Handle config commands early (no DB or dirs needed)
    if let Some(Commands::Config { action }) = &cli.command {
        return cmd_config(action.clone(), &config, cli.config.as_deref());
    }

    // Ensure directories exist for other commands
    config.ensure_dirs()?;

    // Initialize database
    let db_path = directories::ProjectDirs::from("", "", "repoman")
        .map(|d| d.data_dir().join("repoman.db"))
        .unwrap_or_else(|| PathBuf::from("repoman.db"));

    let db = Database::connect(&db_path).await?;

    // Create shared state
    let state: SharedState = Arc::new(RwLock::new(AppState {
        db,
        config: config.clone(),
        aur: AurClient::new(),
        build: BuildSystem::new(config.clone()),
        repo: RepoManager::new(config.clone()),
        build_stages: std::collections::HashMap::new(),
        repo_mutex: Arc::new(tokio::sync::Mutex::new(())),
    }));

    let json_output = cli.json;

    match cli.command {
        Some(Commands::Serve) | None => {
            run_server(state, &config).await?;
        }
        Some(Commands::Add { name, asdeps, with_deps }) => {
            cmd_add(state, &name, asdeps, with_deps, json_output).await?;
        }
        Some(Commands::Remove { name, delete_files, delete_clone, recursive }) => {
            cmd_remove(state, &name, delete_files, delete_clone, recursive).await?;
        }
        Some(Commands::List { status, explicit, deps, orphans }) => {
            cmd_list(state, status, explicit, deps, orphans, json_output).await?;
        }
        Some(Commands::Info { name }) => {
            cmd_info(state, &name, json_output).await?;
        }
        Some(Commands::Diff { name }) => {
            cmd_diff(state, &name).await?;
        }
        Some(Commands::Review { name, stdout }) => {
            cmd_review(state, &name, stdout).await?;
        }
        Some(Commands::Approve { name }) => {
            cmd_approve(state, &name).await?;
        }
        Some(Commands::Build { names, all, updates }) => {
            cmd_build(state, names, all, updates).await?;
        }
        Some(Commands::Logs { id, follow, lines }) => {
            cmd_logs(state, &id, follow, lines).await?;
        }
        Some(Commands::CheckUpdates) => {
            cmd_check_updates(state, json_output).await?;
        }
        Some(Commands::RebuildImage) => {
            cmd_rebuild_image(state).await?;
        }
        Some(Commands::Status) => {
            cmd_status(state, json_output).await?;
        }
        Some(Commands::Mark { name, explicit, asdeps }) => {
            cmd_mark(state, &name, explicit, asdeps).await?;
        }
        Some(Commands::Config { .. }) => {
            unreachable!("Config command handled earlier");
        }
        Some(Commands::Import { db_path, dry_run }) => {
            cmd_import(state, &db_path, dry_run, json_output).await?;
        }
    }

    Ok(())
}

async fn run_server(state: SharedState, config: &Config) -> Result<()> {
    // Check podman availability
    BuildSystem::check_podman()?;

    // Recover from stuck Building/Running states (server crash mid-build)
    {
        let state_read = state.read().await;

        // Find packages stuck in Building status
        let building_packages = state_read
            .db
            .list_packages_by_status(models::PackageStatus::Building)
            .await?;
        for pkg in &building_packages {
            tracing::warn!(
                "Recovering stuck package '{}' from Building to Failed (server restarted)",
                pkg.pkgbase
            );
            if let Err(e) = state_read
                .db
                .update_package_status(&pkg.pkgbase, models::PackageStatus::Failed)
                .await
            {
                tracing::error!("Failed to recover package '{}': {}", pkg.pkgbase, e);
            }
        }

        // Find builds stuck in Running or Pending status
        let all_packages = state_read.db.list_packages().await?;
        for pkg in &all_packages {
            let builds = state_read.db.list_builds(&pkg.pkgbase, 100, 0).await?;
            for build in &builds {
                if build.status == models::BuildStatus::Running
                    || build.status == models::BuildStatus::Pending
                {
                    tracing::warn!(
                        "Recovering stuck build '{}' for '{}' from {:?} to Failed (server restarted)",
                        build.id,
                        pkg.pkgbase,
                        build.status
                    );
                    if let Err(e) = state_read
                        .db
                        .update_build_status(
                            &build.id,
                            models::BuildStatus::Failed,
                            Some("Server restarted during build"),
                        )
                        .await
                    {
                        tracing::error!("Failed to recover build '{}': {}", build.id, e);
                    }
                }
            }
        }

        if !building_packages.is_empty() {
            tracing::info!(
                "Recovered {} package(s) from stuck Building state",
                building_packages.len()
            );
        }
    }

    let app = api::router(state)
        .fallback(assets::static_handler);

    let listener = tokio::net::TcpListener::bind(&config.server.listen).await?;
    tracing::info!("Server listening on {}", config.server.listen);

    axum::serve(listener, app).await?;

    Ok(())
}

// ==================== CLI commands ====================

/// Result of adding a package (used internally for recursive dep tracking)
struct AddedPackage {
    pkgbase: String,
    version: String,
    install_reason: models::InstallReason,
}

async fn cmd_add(state: SharedState, name: &str, asdeps: bool, with_deps: bool, json_output: bool) -> Result<()> {
    // Validate pkgbase format: only [a-zA-Z0-9@._+-] allowed
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '+' | '-'))
    {
        anyhow::bail!(
            "Invalid package name '{}': only alphanumeric characters and @._+- are allowed",
            name
        );
    }

    let mut added_packages: Vec<AddedPackage> = Vec::new();
    let mut failed_deps: Vec<(String, String)> = Vec::new(); // (name, reason)

    {
        let mut state_guard = state.write().await;

        // Check if already exists
        if let Some(pkg) = state_guard.db.get_package(name).await? {
            if json_output {
                println!("{}", serde_json::json!({
                    "status": "already_tracked",
                    "pkgbase": pkg.pkgbase
                }));
            } else {
                println!("Package {} already tracked", pkg.pkgbase);
            }
            return Ok(());
        }

        if let Some(pkgbase) = state_guard.db.resolve_pkgbase(name).await? {
            if json_output {
                println!("{}", serde_json::json!({
                    "status": "already_tracked",
                    "pkgbase": pkgbase
                }));
            } else {
                println!("Package {} already tracked (as {})", name, pkgbase);
            }
            return Ok(());
        }

        // Add the main package
        let install_reason = if asdeps {
            models::InstallReason::Dependency
        } else {
            models::InstallReason::Explicit
        };

        add_package_internal(&mut state_guard, name, install_reason, &mut added_packages, json_output).await?;

        // If --with-deps, recursively add missing AUR dependencies
        if with_deps {
            // Check if base image exists (needed for dep detection)
            let image_exists = state_guard.build.image_exists().unwrap_or(false);
            if !image_exists {
                if !json_output {
                    println!("\nWarning: Base image not found. Cannot detect AUR dependencies.");
                    println!("Run 'repoman rebuild-image' first to enable dependency detection.");
                }
            } else {
                if !json_output {
                    println!("\nChecking for AUR dependencies...");
                }

                // Track which packages we've processed to avoid cycles
                let mut processed: std::collections::HashSet<String> = std::collections::HashSet::new();
                for p in &added_packages {
                    processed.insert(p.pkgbase.clone());
                }

                // Queue of packages to check for deps
                let mut to_check: Vec<String> = added_packages.iter().map(|p| p.pkgbase.clone()).collect();

                while let Some(pkgbase) = to_check.pop() {
                    // Get the srcinfo for this package
                    let clone_path = state_guard.config.resolved_cache().aur_clones.join(&pkgbase);
                    let srcinfo = match aur::read_srcinfo(&clone_path) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    // Get currently tracked package names
                    let tracked_packages = state_guard.db.list_packages().await?;
                    let mut tracked_pkgnames: std::collections::HashSet<String> = std::collections::HashSet::new();
                    for p in &tracked_packages {
                        tracked_pkgnames.insert(p.pkgbase.clone());
                        let names = state_guard.db.get_package_names(&p.pkgbase).await?;
                        for n in names {
                            tracked_pkgnames.insert(n.pkgname);
                        }
                    }

                    // Find missing AUR deps
                    let container_image = &state_guard.config.container.image;
                    let missing = state_guard
                        .aur
                        .find_missing_aur_deps(&srcinfo, &tracked_pkgnames, container_image)
                        .await
                        .unwrap_or_default();

                    // Filter to only deps that exist in AUR and we haven't processed
                    for dep in missing {
                        if !dep.in_aur {
                            failed_deps.push((dep.name.clone(), "not found in AUR".to_string()));
                            continue;
                        }

                        let dep_pkgbase = match &dep.pkgbase {
                            Some(pb) => pb.clone(),
                            None => continue,
                        };

                        if processed.contains(&dep_pkgbase) {
                            continue;
                        }
                        processed.insert(dep_pkgbase.clone());

                        // Add this dependency
                        match add_package_internal(
                            &mut state_guard,
                            &dep.name,
                            models::InstallReason::Dependency,
                            &mut added_packages,
                            json_output,
                        )
                        .await
                        {
                            Ok(_) => {
                                // Queue this package to check for its own deps
                                to_check.push(dep_pkgbase);
                            }
                            Err(e) => {
                                failed_deps.push((dep.name.clone(), e.to_string()));
                            }
                        }
                    }
                }
            }
        }
    }

    // Print summary
    if json_output {
        let added: Vec<serde_json::Value> = added_packages
            .iter()
            .map(|pkg| {
                serde_json::json!({
                    "pkgbase": pkg.pkgbase,
                    "version": pkg.version,
                    "install_reason": match pkg.install_reason {
                        models::InstallReason::Explicit => "explicit",
                        models::InstallReason::Dependency => "dependency",
                    }
                })
            })
            .collect();

        let failed: Vec<serde_json::Value> = failed_deps
            .iter()
            .map(|(name, reason)| {
                serde_json::json!({
                    "name": name,
                    "reason": reason
                })
            })
            .collect();

        println!("{}", serde_json::json!({
            "status": "ok",
            "added": added,
            "failed_deps": failed
        }));
    } else {
        println!();
        if added_packages.is_empty() {
            println!("No packages added.");
        } else {
            println!("Added {} package(s):", added_packages.len());
            for pkg in &added_packages {
                let reason_str = match pkg.install_reason {
                    models::InstallReason::Explicit => "explicit",
                    models::InstallReason::Dependency => "dependency",
                };
                println!("  {} {} ({})", pkg.pkgbase, pkg.version, reason_str);
            }
        }

        if !failed_deps.is_empty() {
            println!("\nFailed to add {} dependency(s):", failed_deps.len());
            for (name, reason) in &failed_deps {
                println!("  {}: {}", name, reason);
            }
        }

        if !added_packages.is_empty() {
            println!("\nPackages need review before building:");
            println!("  repoman list --status pending_review");
            println!("  repoman diff <package>");
            println!("  repoman approve <package>");
        }
    }

    Ok(())
}

/// Internal helper to add a single package without recursion
async fn add_package_internal(
    state: &mut tokio::sync::RwLockWriteGuard<'_, api::AppState>,
    name: &str,
    install_reason: models::InstallReason,
    added_packages: &mut Vec<AddedPackage>,
    json_output: bool,
) -> Result<()> {
    // Check if already exists
    if state.db.get_package(name).await?.is_some() {
        return Ok(()); // Already tracked
    }
    if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
        if state.db.get_package(&pkgbase).await?.is_some() {
            return Ok(()); // Already tracked
        }
    }

    // Query AUR
    let aur_info = state
        .aur
        .info_single(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package '{}' not found in AUR", name))?;

    let pkgbase = &aur_info.package_base;

    // Check again after AUR lookup (pkgbase might differ from name)
    if state.db.get_package(pkgbase).await?.is_some() {
        return Ok(()); // Already tracked
    }

    if !json_output {
        let reason_str = match install_reason {
            models::InstallReason::Explicit => "explicit",
            models::InstallReason::Dependency => "dependency",
        };
        println!("Adding {} ({})...", pkgbase, reason_str);
    }

    // Clone
    let clone_path = state.config.resolved_cache().aur_clones.join(pkgbase);
    if !clone_path.exists() {
        aur::AurGit::clone(pkgbase, &clone_path)?;
    }

    // Parse .SRCINFO
    let srcinfo = aur::read_srcinfo(&clone_path)?;
    let current_commit = aur::AurGit::head_commit(&clone_path)?;
    let version = format!("{}-{}", srcinfo.pkgver, srcinfo.pkgrel);

    // Create package record
    let now = chrono::Utc::now();
    let pkg = models::Package {
        pkgbase: pkgbase.clone(),
        aur_url: format!("https://aur.archlinux.org/{}.git", pkgbase),
        local_path: clone_path.to_string_lossy().to_string(),
        approved_commit: None,
        current_commit: Some(current_commit),
        pkgver: Some(version.clone()),
        status: models::PackageStatus::PendingReview,
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
        pkg_names.push(models::PackageName {
            pkgname: sp.pkgname.clone(),
            pkgbase: pkgbase.clone(),
            pkgdesc: sp.pkgdesc.clone().or(srcinfo.pkgdesc.clone()),
            pkgver: Some(version.clone()),
            depends: Some(serde_json::to_string(&sp.depends)?),
            optdepends: Some(serde_json::to_string(&sp.optdepends)?),
            provides: Some(serde_json::to_string(&sp.provides)?),
            conflicts: Some(serde_json::to_string(&sp.conflicts)?),
            repo_version: None,
        });
    }

    // Insert package and its names atomically
    state.db.insert_package_with_names(&pkg, &pkg_names).await?;

    added_packages.push(AddedPackage {
        pkgbase: pkgbase.clone(),
        version,
        install_reason,
    });

    Ok(())
}

async fn cmd_remove(state: SharedState, name: &str, delete_files: bool, delete_clone: bool, recursive: bool) -> Result<()> {
    let state = state.write().await;

    let pkg = state.db.get_package(name).await?
        .or({
            if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
                state.db.get_package(&pkgbase).await?
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    if delete_files {
        let names = state.db.get_package_names(&pkg.pkgbase).await?;
        let pkgnames: Vec<&str> = names.iter().map(|n| n.pkgname.as_str()).collect();
        println!("Removing from repo: {:?}", pkgnames);
        state.build.repo_remove(&pkgnames)?;

        for name in &names {
            state.repo.delete_package_files(&name.pkgname)?;
        }
    }

    if delete_clone {
        let clone_path = std::path::Path::new(&pkg.local_path);
        if clone_path.exists() {
            println!("Deleting clone: {}", clone_path.display());
            std::fs::remove_dir_all(clone_path)?;
        }
        // Also clean up cached sources
        state.build.cleanup_all_sources(&pkg.pkgbase)?;
    }

    state.db.delete_package(&pkg.pkgbase).await?;
    println!("Removed {}", pkg.pkgbase);

    // If recursive, also remove orphaned dependencies
    if recursive {
        let orphans = state.db.list_orphans().await?;
        if !orphans.is_empty() {
            println!("\nRemoving orphaned dependencies:");
            for orphan in orphans {
                if delete_files {
                    let names = state.db.get_package_names(&orphan.pkgbase).await?;
                    let pkgnames: Vec<&str> = names.iter().map(|n| n.pkgname.as_str()).collect();
                    state.build.repo_remove(&pkgnames)?;
                    for name in &names {
                        state.repo.delete_package_files(&name.pkgname)?;
                    }
                }
                if delete_clone {
                    let clone_path = std::path::Path::new(&orphan.local_path);
                    if clone_path.exists() {
                        std::fs::remove_dir_all(clone_path)?;
                    }
                    // Also clean up cached sources
                    state.build.cleanup_all_sources(&orphan.pkgbase)?;
                }
                state.db.delete_package(&orphan.pkgbase).await?;
                println!("  Removed {} (orphan)", orphan.pkgbase);
            }
        }
    }

    Ok(())
}

async fn cmd_list(state: SharedState, status: Option<String>, explicit: bool, deps: bool, orphans_flag: bool, json_output: bool) -> Result<()> {
    let state = state.read().await;

    // Get packages based on filters
    let packages = if orphans_flag {
        state.db.list_orphans().await?
    } else if explicit {
        state.db.list_packages_by_reason(models::InstallReason::Explicit).await?
    } else if deps {
        state.db.list_packages_by_reason(models::InstallReason::Dependency).await?
    } else if let Some(ref s) = status {
        let status = match s.as_str() {
            "pending_review" => models::PackageStatus::PendingReview,
            "approved" => models::PackageStatus::Approved,
            "building" => models::PackageStatus::Building,
            "failed" => models::PackageStatus::Failed,
            "up_to_date" => models::PackageStatus::UpToDate,
            "update_available" => models::PackageStatus::UpdateAvailable,
            "rejected" => models::PackageStatus::Rejected,
            _ => anyhow::bail!("Invalid status: {}", s),
        };
        state.db.list_packages_by_status(status).await?
    } else {
        state.db.list_packages().await?
    };

    if json_output {
        let json_packages: Vec<serde_json::Value> = packages
            .iter()
            .map(|pkg| {
                serde_json::json!({
                    "pkgbase": pkg.pkgbase,
                    "version": pkg.pkgver,
                    "status": pkg.status.to_string(),
                    "install_reason": pkg.install_reason.to_string(),
                    "auto_approve": pkg.auto_approve,
                    "current_commit": pkg.current_commit,
                    "approved_commit": pkg.approved_commit,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_packages)?);
        return Ok(());
    }

    if packages.is_empty() {
        if orphans_flag {
            println!("No orphaned packages found");
        } else {
            println!("No packages found");
        }
        return Ok(());
    }

    println!("{:<30} {:<20} {:<15} {:<10}", "PKGBASE", "VERSION", "STATUS", "REASON");
    println!("{}", "-".repeat(75));

    for pkg in &packages {
        println!("{:<30} {:<20} {:<15} {:<10}",
            pkg.pkgbase,
            pkg.pkgver.as_deref().unwrap_or("-"),
            pkg.status,
            pkg.install_reason
        );
    }

    println!("\nTotal: {} package(s)", packages.len());

    Ok(())
}

async fn cmd_info(state: SharedState, name: &str, json_output: bool) -> Result<()> {
    let state = state.read().await;

    let pkg = state.db.get_package(name).await?
        .or({
            if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
                state.db.get_package(&pkgbase).await?
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let names = state.db.get_package_names(&pkg.pkgbase).await?;

    if json_output {
        let pkgnames: Vec<serde_json::Value> = names
            .iter()
            .map(|n| {
                serde_json::json!({
                    "pkgname": n.pkgname,
                    "pkgdesc": n.pkgdesc,
                    "pkgver": n.pkgver,
                    "depends": n.depends.as_ref().and_then(|d| serde_json::from_str::<Vec<String>>(d).ok()),
                    "provides": n.provides.as_ref().and_then(|p| serde_json::from_str::<Vec<String>>(p).ok()),
                    "conflicts": n.conflicts.as_ref().and_then(|c| serde_json::from_str::<Vec<String>>(c).ok()),
                    "repo_version": n.repo_version,
                })
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "pkgbase": pkg.pkgbase,
            "version": pkg.pkgver,
            "status": pkg.status.to_string(),
            "install_reason": pkg.install_reason.to_string(),
            "auto_approve": pkg.auto_approve,
            "aur_url": pkg.aur_url,
            "local_path": pkg.local_path,
            "current_commit": pkg.current_commit,
            "approved_commit": pkg.approved_commit,
            "added_at": pkg.added_at.to_rfc3339(),
            "updated_at": pkg.updated_at.to_rfc3339(),
            "packages": pkgnames,
        }))?);
        return Ok(());
    }

    println!("Package: {}", pkg.pkgbase);
    println!("Version: {}", pkg.pkgver.as_deref().unwrap_or("-"));
    println!("Status: {}", pkg.status);
    println!("Install reason: {}", pkg.install_reason);
    println!("Auto-approve: {}", pkg.auto_approve);
    println!("URL: {}", pkg.aur_url);
    println!("Local path: {}", pkg.local_path);
    println!("Current commit: {}", pkg.current_commit.as_deref().unwrap_or("-"));
    println!("Approved commit: {}", pkg.approved_commit.as_deref().unwrap_or("-"));
    println!("Added: {}", pkg.added_at);
    println!("Updated: {}", pkg.updated_at);
    println!();
    println!("Packages produced:");
    for name in names {
        println!("  - {} ({})", name.pkgname, name.pkgdesc.as_deref().unwrap_or("-"));
    }

    Ok(())
}

async fn cmd_diff(state: SharedState, name: &str) -> Result<()> {
    let state = state.read().await;

    let pkg = state.db.get_package(name).await?
        .or({
            if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
                state.db.get_package(&pkgbase).await?
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let clone_path = std::path::Path::new(&pkg.local_path);

    if let Some(ref approved) = pkg.approved_commit {
        let current = pkg.current_commit.as_deref().unwrap_or("HEAD");
        let diff = aur::AurGit::diff(clone_path, approved, current)?;

        if diff.is_empty() {
            println!("No changes since last approval");
        } else {
            println!("{}", diff);
        }
    } else {
        // Show PKGBUILD content
        let pkgbuild = std::fs::read_to_string(clone_path.join("PKGBUILD"))?;
        println!("=== PKGBUILD (not yet approved) ===");
        println!("{}", pkgbuild);
    }

    Ok(())
}

async fn cmd_review(state: SharedState, name: &str, stdout: bool) -> Result<()> {
    let state = state.read().await;

    let pkg = state.db.get_package(name).await?
        .or({
            if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
                state.db.get_package(&pkgbase).await?
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let clone_path = std::path::Path::new(&pkg.local_path);
    let pkgbuild_path = clone_path.join("PKGBUILD");

    if !pkgbuild_path.exists() {
        anyhow::bail!("PKGBUILD not found at {}", pkgbuild_path.display());
    }

    if stdout {
        // Print to stdout
        let content = std::fs::read_to_string(&pkgbuild_path)?;
        println!("{}", content);
    } else {
        // Open in $EDITOR
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| "vi".to_string());

        let status = std::process::Command::new(&editor)
            .arg(&pkgbuild_path)
            .status()?;

        if !status.success() {
            anyhow::bail!("Editor exited with non-zero status");
        }

        println!("\nReviewed: {}", pkg.pkgbase);
        println!("Status: {}", pkg.status);
        if pkg.status == models::PackageStatus::PendingReview || pkg.status == models::PackageStatus::UpdateAvailable {
            println!("\nTo approve: repoman approve {}", pkg.pkgbase);
        }
    }

    Ok(())
}

async fn cmd_approve(state: SharedState, name: &str) -> Result<()> {
    let state = state.write().await;

    let pkg = state.db.get_package(name).await?
        .or({
            if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
                state.db.get_package(&pkgbase).await?
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    state.db.approve_package(&pkg.pkgbase, false, None).await?;
    println!("Approved {} at commit {}", pkg.pkgbase, pkg.current_commit.as_deref().unwrap_or("?"));
    println!("Run 'repoman build {}' to build", pkg.pkgbase);

    Ok(())
}

async fn cmd_build(state: SharedState, names: Vec<String>, all: bool, updates: bool) -> Result<()> {
    // Determine which packages to build
    let packages_to_build: Vec<String> = {
        let state_read = state.read().await;

        if all {
            // Build all approved packages
            let approved = state_read.db.list_packages_by_status(models::PackageStatus::Approved).await?;
            let update_avail = state_read.db.list_packages_by_status(models::PackageStatus::UpdateAvailable).await?;
            approved.into_iter().chain(update_avail).map(|p| p.pkgbase).collect()
        } else if updates {
            // Build packages with updates
            state_read.db.list_packages_by_status(models::PackageStatus::UpdateAvailable).await?
                .into_iter().map(|p| p.pkgbase).collect()
        } else {
            // Resolve provided names to pkgbases
            let mut resolved = Vec::new();
            for name in &names {
                if let Some(pkg) = state_read.db.get_package(name).await? {
                    resolved.push(pkg.pkgbase);
                } else if let Some(pkgbase) = state_read.db.resolve_pkgbase(name).await? {
                    resolved.push(pkgbase);
                } else {
                    println!("Warning: package '{}' not found, skipping", name);
                }
            }
            resolved
        }
    };

    if packages_to_build.is_empty() {
        println!("No packages to build");
        return Ok(());
    }

    // Single package: use direct build (synchronous, shows output)
    if packages_to_build.len() == 1 {
        return cmd_build_single(state, &packages_to_build[0]).await;
    }

    // Multiple packages: use the queue system
    println!("Queuing {} packages for build...", packages_to_build.len());

    // Build dependency graph for topological sort
    let (build_order, deps_graph, _pkg_infos) = {
        let state_read = state.read().await;

        // Check base image exists
        if !state_read.build.image_exists()? {
            anyhow::bail!("Base image not found. Run 'repoman rebuild-image' first.");
        }

        // Get packages and validate
        let mut valid_packages: Vec<models::Package> = Vec::new();
        for pkgbase in &packages_to_build {
            if let Some(pkg) = state_read.db.get_package(pkgbase).await? {
                if pkg.status == models::PackageStatus::Approved || pkg.status == models::PackageStatus::UpdateAvailable {
                    valid_packages.push(pkg);
                } else {
                    println!("Skipping {}: status is '{}' (must be approved)", pkgbase, pkg.status);
                }
            }
        }

        if valid_packages.is_empty() {
            println!("No packages ready to build");
            return Ok(());
        }

        // Build pkgname -> pkgbase mapping
        let pkgbases_set: std::collections::HashSet<String> = valid_packages.iter().map(|p| p.pkgbase.clone()).collect();
        let mut pkgname_to_pkgbase: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for pkg in &valid_packages {
            let names = state_read.db.get_package_names(&pkg.pkgbase).await?;
            for name in names {
                pkgname_to_pkgbase.insert(name.pkgname, pkg.pkgbase.clone());
            }
        }

        // Build dependency graph
        let mut deps_graph: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
        for pkg in &valid_packages {
            let names = state_read.db.get_package_names(&pkg.pkgbase).await?;
            let mut pkg_deps: Vec<String> = Vec::new();
            for name in &names {
                if let Some(deps_json) = &name.depends {
                    if let Ok(deps) = serde_json::from_str::<Vec<String>>(deps_json) {
                        for dep in deps {
                            let dep_name = aur::DepResolver::dep_name(&dep);
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

        // Topological sort
        let build_order = topological_sort_cli(&pkgbases_set, &deps_graph)?;

        // Collect package info
        let pkg_infos: Vec<(String, String)> = valid_packages.iter()
            .map(|p| (p.pkgbase.clone(), p.local_path.clone()))
            .collect();

        (build_order, deps_graph, pkg_infos)
    };

    println!("\nBuild order (dependencies first):");
    for (i, pkgbase) in build_order.iter().enumerate() {
        println!("  {}. {}", i + 1, pkgbase);
    }
    println!();

    // Run builds sequentially
    let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut succeeded = 0;
    let mut skipped = 0;

    for pkgbase in &build_order {
        // Check if any dependency failed
        let should_skip = if let Some(pkg_deps) = deps_graph.get(pkgbase) {
            pkg_deps.iter().any(|dep| failed.contains(dep))
        } else {
            false
        };

        if should_skip {
            println!("Skipping {} (dependency failed)", pkgbase);
            failed.insert(pkgbase.clone());
            skipped += 1;
            continue;
        }

        println!("\n=== Building {} ===", pkgbase);

        match cmd_build_single(state.clone(), pkgbase).await {
            Ok(_) => {
                succeeded += 1;
            }
            Err(e) => {
                println!("Build failed for {}: {}", pkgbase, e);
                failed.insert(pkgbase.clone());
            }
        }
    }

    println!("\n=== Build queue complete ===");
    println!("Succeeded: {}", succeeded);
    println!("Failed: {}", failed.len() - skipped);
    println!("Skipped (dependency failed): {}", skipped);

    if !failed.is_empty() {
        anyhow::bail!("{} build(s) failed", failed.len());
    }

    Ok(())
}

/// Topological sort for CLI (Kahn's algorithm)
fn topological_sort_cli(
    nodes: &std::collections::HashSet<String>,
    deps: &std::collections::HashMap<String, Vec<String>>,
) -> Result<Vec<String>> {
    use std::collections::{HashMap, HashSet, VecDeque};

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
        anyhow::bail!("Circular dependency detected in build queue");
    }

    Ok(result)
}

/// Build a single package (synchronous, with output)
async fn cmd_build_single(state: SharedState, name: &str) -> Result<()> {
    let (pkg, build_system, log_path) = {
        let state_read = state.read().await;

        let pkg = state_read.db.get_package(name).await?
            .or({
                if let Some(pkgbase) = state_read.db.resolve_pkgbase(name).await? {
                    state_read.db.get_package(&pkgbase).await?
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

        if pkg.status != models::PackageStatus::Approved && pkg.status != models::PackageStatus::UpdateAvailable {
            anyhow::bail!("Package must be approved before building (current status: {})", pkg.status);
        }

        if !state_read.build.image_exists()? {
            anyhow::bail!("Base image not found. Run 'repoman rebuild-image' first.");
        }

        let log_path = state_read.config.resolved_cache().logs.join(format!("{}-build.log", pkg.pkgbase));
        let build_system = build::BuildSystem::new(state_read.config.clone());

        std::fs::create_dir_all(&state_read.config.repo.path)?;
        build_system.init_repo()?;

        (pkg, build_system, log_path)
    };

    {
        let state_write = state.write().await;
        state_write.db.update_package_status(&pkg.pkgbase, models::PackageStatus::Building).await?;
    }

    println!("Building {}...", pkg.pkgbase);

    let source_path = std::path::Path::new(&pkg.local_path);
    let result = build_system.run_build_sync(&pkg.pkgbase, source_path, &log_path);

    match result {
        Ok(packages) => {
            println!("Moving packages to repository...");
            let post_build_result = (|| -> Result<Vec<PathBuf>> {
                let repo_packages = build_system.move_to_repo(&packages)?;
                build_system.repo_add(&repo_packages)?;
                Ok(repo_packages)
            })();

            match post_build_result {
                Ok(repo_packages) => {
                    {
                        let state_write = state.write().await;
                        state_write.db.update_package_status(&pkg.pkgbase, models::PackageStatus::UpToDate).await?;
                    }

                    println!("\nBuild successful!");
                    println!("Packages added to repository:");
                    for pkg in &repo_packages {
                        println!("  - {}", pkg.file_name().unwrap_or_default().to_string_lossy());
                    }
                    Ok(())
                }
                Err(e) => {
                    {
                        let state_write = state.write().await;
                        state_write.db.update_package_status(&pkg.pkgbase, models::PackageStatus::Failed).await?;
                    }
                    Err(e)
                }
            }
        }
        Err(e) => {
            {
                let state_write = state.write().await;
                state_write.db.update_package_status(&pkg.pkgbase, models::PackageStatus::Failed).await?;
            }

            println!("\nBuild failed: {}", e);
            println!("Check logs at: {}", log_path.display());
            anyhow::bail!("Build failed");
        }
    }
}

async fn cmd_logs(state: SharedState, id: &str, follow: bool, lines: Option<usize>) -> Result<()> {
    let state = state.read().await;

    // Try to find the log path
    // Determine the log path
    let log_path: PathBuf = if let Some(build) = state.db.get_build(id).await? {
        // Found a build by ID
        if let Some(path) = build.log_path {
            PathBuf::from(path)
        } else {
            anyhow::bail!("Build {} has no log file", id);
        }
    } else {
        // Try to interpret as package name and get latest build
        let pkgbase: Option<String> = if let Some(pkg) = state.db.get_package(id).await? {
            Some(pkg.pkgbase)
        } else if let Some(resolved) = state.db.resolve_pkgbase(id).await? {
            Some(resolved)
        } else {
            None
        };

        if let Some(pkgbase) = pkgbase {
            // Get the latest build for this package
            let builds = state.db.list_builds(&pkgbase, 1, 0).await?;
            if let Some(build) = builds.first() {
                if let Some(path) = &build.log_path {
                    PathBuf::from(path)
                } else {
                    // Fallback to default log path
                    state.config.resolved_cache().logs.join(format!("{}-build.log", pkgbase))
                }
            } else {
                // No builds yet, try the default log path
                state.config.resolved_cache().logs.join(format!("{}-build.log", pkgbase))
            }
        } else {
            // Try direct log file path as fallback
            let direct_log = state.config.resolved_cache().logs.join(format!("{}-build.log", id));
            if direct_log.exists() {
                direct_log
            } else {
                anyhow::bail!("No build or package found for '{}'", id);
            }
        }
    };

    if !log_path.exists() {
        anyhow::bail!("Log file not found: {}", log_path.display());
    }

    if follow {
        // Follow mode: continuously read new lines
        use std::io::{BufRead, BufReader, Seek, SeekFrom};

        let file = std::fs::File::open(&log_path)?;
        let mut reader = BufReader::new(file);

        // If lines is specified, seek to show only that many lines initially
        if let Some(n) = lines {
            let content = std::fs::read_to_string(&log_path)?;
            let total_lines: Vec<&str> = content.lines().collect();
            let skip = total_lines.len().saturating_sub(n);
            for line in total_lines.iter().skip(skip) {
                println!("{}", line);
            }

            // Seek to end for follow
            reader.seek(SeekFrom::End(0))?;
        }

        println!("--- Following log (Ctrl+C to stop) ---");

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // No new content, wait a bit
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Ok(_) => {
                    print!("{}", line);
                }
                Err(e) => {
                    eprintln!("Error reading log: {}", e);
                    break;
                }
            }
        }
    } else {
        // Regular mode: print the log
        let content = std::fs::read_to_string(&log_path)?;

        if let Some(n) = lines {
            // Show last n lines
            let all_lines: Vec<&str> = content.lines().collect();
            let skip = all_lines.len().saturating_sub(n);
            for line in all_lines.iter().skip(skip) {
                println!("{}", line);
            }
        } else {
            // Show all
            print!("{}", content);
        }
    }

    Ok(())
}

async fn cmd_check_updates(state: SharedState, json_output: bool) -> Result<()> {
    let state = state.write().await;

    let packages = state.db.list_packages().await?;
    let mut updates: Vec<serde_json::Value> = Vec::new();

    for pkg in packages {
        // Don't touch the source directory while a build is actively using it
        if pkg.status == models::PackageStatus::Building {
            if !json_output {
                println!("Skipping {} (build in progress)", pkg.pkgbase);
            }
            continue;
        }

        let clone_path = std::path::Path::new(&pkg.local_path);

        if !json_output {
            print!("Checking {}... ", pkg.pkgbase);
        }

        if aur::AurGit::has_updates(clone_path)? {
            // Read old SRCINFO before resetting to remote
            let old_srcinfo = aur::read_srcinfo(clone_path).ok();
            let old_version = old_srcinfo.as_ref().map(|s| s.full_version());

            aur::AurGit::reset_to_remote(clone_path)?;
            let new_commit = aur::AurGit::head_commit(clone_path)?;

            let srcinfo = aur::read_srcinfo(clone_path)?;
            let pkgver = format!("{}-{}", srcinfo.pkgver, srcinfo.pkgrel);

            // Clean up stale sources if version changed
            if old_version.as_deref() != Some(&pkgver) {
                let deleted = state.build.cleanup_stale_sources(
                    &pkg.pkgbase,
                    old_srcinfo.as_ref(),
                    &srcinfo,
                )?;
                if !deleted.is_empty() && !json_output {
                    println!("Cleaned {} stale source(s)", deleted.len());
                    print!("  ");
                }
            }

            state.db.update_package_commit(&pkg.pkgbase, &new_commit, Some(&pkgver)).await?;

            let new_status = if pkg.auto_approve {
                state.db.approve_package(&pkg.pkgbase, true, None).await?;
                models::PackageStatus::Approved
            } else {
                models::PackageStatus::UpdateAvailable
            };

            state.db.update_package_status(&pkg.pkgbase, new_status).await?;

            if !json_output {
                println!("UPDATE AVAILABLE ({})", pkgver);
            }

            updates.push(serde_json::json!({
                "pkgbase": pkg.pkgbase,
                "old_version": old_version,
                "new_version": pkgver,
                "auto_approved": pkg.auto_approve,
            }));
        } else if !json_output {
            println!("up to date");
        }
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "updates": updates,
            "count": updates.len(),
        }))?);
    } else if updates.is_empty() {
        println!("\nAll packages up to date");
    } else {
        let names: Vec<&str> = updates.iter()
            .filter_map(|u| u.get("pkgbase").and_then(|v| v.as_str()))
            .collect();
        println!("\n{} package(s) with updates: {}", updates.len(), names.join(", "));
    }

    Ok(())
}

async fn cmd_rebuild_image(state: SharedState) -> Result<()> {
    let state = state.read().await;

    println!("Rebuilding base container image...");
    state.build.build_base_image()?;
    println!("Done");

    Ok(())
}

async fn cmd_status(state: SharedState, json_output: bool) -> Result<()> {
    let state = state.read().await;

    // Gather status info
    let podman_available = BuildSystem::check_podman().is_ok();
    let image_exists = state.build.image_exists().unwrap_or(false);
    let repo_stats = state.repo.stats().ok();

    // Package counts
    let packages = state.db.list_packages().await?;
    let mut by_status: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for pkg in &packages {
        *by_status.entry(pkg.status.to_string()).or_default() += 1;
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "podman": {
                "available": podman_available,
            },
            "image": {
                "name": state.config.container.image,
                "exists": image_exists,
            },
            "repository": repo_stats.as_ref().map(|s| serde_json::json!({
                "path": s.path.to_string_lossy(),
                "package_count": s.package_count,
                "total_size": s.total_size,
            })),
            "packages": {
                "total": packages.len(),
                "by_status": by_status,
            },
        }))?);
        return Ok(());
    }

    // Check podman
    print!("Podman: ");
    if podman_available {
        println!("available");
    } else {
        println!("NOT AVAILABLE");
    }

    // Check base image
    print!("Base image ({}): ", state.config.container.image);
    if image_exists {
        println!("exists");
    } else {
        println!("NOT FOUND (run 'repoman rebuild-image')");
    }

    // Repo stats
    match repo_stats {
        Some(stats) => {
            println!("Repository: {} ({} packages, {} bytes)",
                stats.path.display(),
                stats.package_count,
                stats.total_size
            );
        }
        None => println!("Repository: error"),
    }

    println!("\nPackages ({} total):", packages.len());
    for (status, count) in by_status {
        println!("  {}: {}", status, count);
    }

    Ok(())
}

async fn cmd_mark(state: SharedState, name: &str, explicit: bool, asdeps: bool) -> Result<()> {
    let state = state.write().await;

    let pkg = state.db.get_package(name).await?
        .or({
            if let Some(pkgbase) = state.db.resolve_pkgbase(name).await? {
                state.db.get_package(&pkgbase).await?
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let new_reason = if explicit {
        models::InstallReason::Explicit
    } else if asdeps {
        models::InstallReason::Dependency
    } else {
        anyhow::bail!("Must specify either --explicit or --asdeps");
    };

    if pkg.install_reason == new_reason {
        println!("{} is already marked as {}", pkg.pkgbase, new_reason);
        return Ok(());
    }

    state.db.update_install_reason(&pkg.pkgbase, new_reason).await?;
    println!("Changed {} install reason: {} -> {}", pkg.pkgbase, pkg.install_reason, new_reason);

    Ok(())
}

fn cmd_config(action: ConfigAction, config: &Config, custom_path: Option<&std::path::Path>) -> Result<()> {
    match action {
        ConfigAction::Show => {
            println!("{}", config.to_toml()?);
        }
        ConfigAction::Template => {
            print!("{}", Config::template());
        }
        ConfigAction::Path => {
            let path = custom_path
                .map(PathBuf::from)
                .or_else(|| {
                    directories::ProjectDirs::from("", "", "repoman")
                        .map(|dirs| dirs.config_dir().join("config.toml"))
                })
                .or_else(|| dirs::home_dir().map(|h| h.join(".config/repoman/config.toml")));

            if let Some(p) = path {
                println!("{}", p.display());
                if p.exists() {
                    println!("(file exists)");
                } else {
                    println!("(file does not exist)");
                }
            } else {
                println!("Could not determine config path");
            }
        }
    }
    Ok(())
}

/// Package being imported (intermediate state)
struct ImportedPackage {
    pkg: models::Package,
    pkg_entries: Vec<repo::db_parser::RepoPkgEntry>,
    pkg_names: Vec<models::PackageName>,
}

async fn cmd_import(state: SharedState, db_path: &std::path::Path, dry_run: bool, json_output: bool) -> Result<()> {
    use std::collections::HashSet;
    use repo::db_parser::parse_repo_db;
    use anyhow::Context;

    // Parse the repository database
    if !json_output {
        println!("Parsing repository database: {}", db_path.display());
    }

    let repo_contents = parse_repo_db(db_path)
        .with_context(|| format!("Failed to parse repo database: {}", db_path.display()))?;

    if !json_output {
        println!("Found {} packages in {} pkgbases",
            repo_contents.packages.len(),
            repo_contents.by_pkgbase.len());
    }

    // Get state for reading tracked packages
    let (config, tracked_pkgbases) = {
        let state_read = state.read().await;
        let packages = state_read.db.list_packages().await?;
        let tracked: HashSet<String> = packages.iter().map(|p| p.pkgbase.clone()).collect();
        (state_read.config.clone(), tracked)
    };

    // Process each pkgbase
    let mut imported: Vec<ImportedPackage> = Vec::new();
    let mut skipped_tracked = 0;
    let mut no_upstream = 0;
    let mut clone_errors: Vec<(String, String)> = Vec::new();

    let total = repo_contents.by_pkgbase.len();

    for (idx, (pkgbase, entries)) in repo_contents.by_pkgbase.iter().enumerate() {
        // Skip already tracked packages
        if tracked_pkgbases.contains(pkgbase) {
            skipped_tracked += 1;
            continue;
        }

        if !json_output {
            print!("[{}/{}] {} ", idx + 1, total, pkgbase);
            use std::io::Write;
            std::io::stdout().flush().ok();
        }

        // Get version from repo
        let repo_version = entries.first().map(|e| e.version.clone()).unwrap_or_default();

        // Try to clone from AUR
        let clone_path = config.resolved_cache().aur_clones.join(pkgbase);

        let (has_upstream, srcinfo_opt, current_commit) = if !dry_run {
            // Actually try to clone
            if !clone_path.exists() {
                match aur::AurGit::clone(pkgbase, &clone_path) {
                    Ok(_) => {
                        let srcinfo = aur::read_srcinfo(&clone_path).ok();
                        let commit = aur::AurGit::head_commit(&clone_path).ok();
                        if !json_output {
                            print!("(cloned) ");
                        }
                        (true, srcinfo, commit)
                    }
                    Err(e) => {
                        if !json_output {
                            print!("(no AUR upstream) ");
                        }
                        clone_errors.push((pkgbase.to_string(), e.to_string()));
                        no_upstream += 1;
                        (false, None, None)
                    }
                }
            } else {
                // Already cloned
                let srcinfo = aur::read_srcinfo(&clone_path).ok();
                let commit = aur::AurGit::head_commit(&clone_path).ok();
                (true, srcinfo, commit)
            }
        } else {
            // Dry run: check AUR RPC to see if package exists
            let state_read = state.read().await;
            match state_read.aur.info_single(pkgbase).await {
                Ok(Some(_)) => {
                    if !json_output {
                        print!("(in AUR) ");
                    }
                    (true, None, None)
                }
                _ => {
                    if !json_output {
                        print!("(not in AUR) ");
                    }
                    no_upstream += 1;
                    (false, None, None)
                }
            }
        };

        // Determine status based on version comparison
        let status = if !has_upstream {
            // No upstream, mark as up to date (can't update)
            models::PackageStatus::UpToDate
        } else if let Some(ref srcinfo) = srcinfo_opt {
            let aur_version = srcinfo.full_version();
            if aur_version == repo_version {
                models::PackageStatus::UpToDate
            } else {
                if !json_output {
                    print!("(update: {} -> {}) ", repo_version, aur_version);
                }
                models::PackageStatus::UpdateAvailable
            }
        } else {
            // Dry run or couldn't read srcinfo, assume up to date
            models::PackageStatus::UpToDate
        };

        if !json_output {
            println!();
        }

        // Create package record
        let now = chrono::Utc::now();
        let aur_version = srcinfo_opt.as_ref().map(|s| s.full_version());

        let pkg = models::Package {
            pkgbase: pkgbase.to_string(),
            aur_url: format!("https://aur.archlinux.org/{}.git", pkgbase),
            local_path: clone_path.to_string_lossy().to_string(),
            // Auto-approve imported packages (they were already built)
            approved_commit: current_commit.clone(),
            current_commit: current_commit.clone(),
            pkgver: aur_version.or(Some(repo_version.clone())),
            status,
            install_reason: models::InstallReason::Explicit, // Will be updated later
            auto_approve: false,
            has_upstream,
            in_official_repos: false, // Will be updated later
            rejection_reason: None,
            added_at: now,
            updated_at: now,
        };

        // Create package names from repo entries or srcinfo
        let mut pkg_names = Vec::new();
        if let Some(ref srcinfo) = srcinfo_opt {
            // Use srcinfo for more accurate data
            for sp in &srcinfo.packages {
                // Merge global and per-package deps
                let mut all_depends = srcinfo.depends.clone();
                all_depends.extend(sp.depends.clone());

                let mut all_optdepends = srcinfo.optdepends.clone();
                all_optdepends.extend(sp.optdepends.clone());

                let mut all_provides = srcinfo.provides.clone();
                all_provides.extend(sp.provides.clone());

                let mut all_conflicts = srcinfo.conflicts.clone();
                all_conflicts.extend(sp.conflicts.clone());

                pkg_names.push(models::PackageName {
                    pkgname: sp.pkgname.clone(),
                    pkgbase: pkgbase.to_string(),
                    pkgdesc: sp.pkgdesc.clone().or(srcinfo.pkgdesc.clone()),
                    pkgver: Some(srcinfo.full_version()),
                    depends: Some(serde_json::to_string(&all_depends)?),
                    optdepends: Some(serde_json::to_string(&all_optdepends)?),
                    provides: Some(serde_json::to_string(&all_provides)?),
                    conflicts: Some(serde_json::to_string(&all_conflicts)?),
                    repo_version: Some(repo_version.clone()),
                });
            }
        } else {
            // Use repo database entries
            for entry in entries {
                pkg_names.push(models::PackageName {
                    pkgname: entry.name.clone(),
                    pkgbase: pkgbase.to_string(),
                    pkgdesc: entry.desc.clone(),
                    pkgver: Some(entry.version.clone()),
                    depends: Some(serde_json::to_string(&entry.depends)?),
                    optdepends: Some(serde_json::to_string(&entry.optdepends)?),
                    provides: Some(serde_json::to_string(&entry.provides)?),
                    conflicts: Some(serde_json::to_string(&entry.conflicts)?),
                    repo_version: Some(entry.version.clone()),
                });
            }
        }

        imported.push(ImportedPackage {
            pkg,
            pkg_entries: entries.clone(),
            pkg_names,
        });
    }

    // Infer install reasons from dependency graph
    infer_install_reasons(&mut imported);

    // Count stats
    let explicit_count = imported.iter().filter(|p| p.pkg.install_reason == models::InstallReason::Explicit).count();
    let dep_count = imported.iter().filter(|p| p.pkg.install_reason == models::InstallReason::Dependency).count();
    let update_count = imported.iter().filter(|p| p.pkg.status == models::PackageStatus::UpdateAvailable).count();
    let deprecated_count = imported.iter().filter(|p| !p.pkg.has_upstream && p.pkg.in_official_repos).count();
    let local_only_count = imported.iter().filter(|p| !p.pkg.has_upstream && !p.pkg.in_official_repos).count();

    if dry_run {
        // Print what would be imported
        if json_output {
            let packages: Vec<serde_json::Value> = imported.iter().map(|imp| {
                serde_json::json!({
                    "pkgbase": imp.pkg.pkgbase,
                    "version": imp.pkg.pkgver,
                    "status": imp.pkg.status.to_string(),
                    "install_reason": imp.pkg.install_reason.to_string(),
                    "has_upstream": imp.pkg.has_upstream,
                    "in_official_repos": imp.pkg.in_official_repos,
                    "pkgnames": imp.pkg_names.iter().map(|n| &n.pkgname).collect::<Vec<_>>(),
                })
            }).collect();

            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": true,
                "would_import": packages.len(),
                "skipped_already_tracked": skipped_tracked,
                "explicit": explicit_count,
                "dependencies": dep_count,
                "updates_available": update_count,
                "no_upstream": no_upstream,
                "deprecated": deprecated_count,
                "local_only": local_only_count,
                "packages": packages,
            }))?);
        } else {
            println!("\n=== Dry run summary ===");
            println!("Would import: {} packages", imported.len());
            println!("  Explicit: {}", explicit_count);
            println!("  Dependencies: {}", dep_count);
            println!("  Updates available: {}", update_count);
            println!("  No AUR upstream: {}", no_upstream);
            if deprecated_count > 0 {
                println!("    - Deprecated (now in official): {}", deprecated_count);
            }
            if local_only_count > 0 {
                println!("    - Local only (not in AUR or official): {}", local_only_count);
            }
            println!("Skipped (already tracked): {}", skipped_tracked);

            if !imported.is_empty() {
                println!("\nPackages to import:");
                for imp in &imported {
                    let reason = match imp.pkg.install_reason {
                        models::InstallReason::Explicit => "explicit",
                        models::InstallReason::Dependency => "dependency",
                    };
                    let status_note = if imp.pkg.status == models::PackageStatus::UpdateAvailable {
                        " (update available)"
                    } else if !imp.pkg.has_upstream && imp.pkg.in_official_repos {
                        " (DEPRECATED - now in official repos)"
                    } else if !imp.pkg.has_upstream {
                        " (local only)"
                    } else {
                        ""
                    };
                    println!("  {} {} [{}]{}",
                        imp.pkg.pkgbase,
                        imp.pkg.pkgver.as_deref().unwrap_or("-"),
                        reason,
                        status_note);
                }
            }
        }

        return Ok(());
    }

    // Actually insert packages
    if !json_output {
        println!("\nInserting packages into database...");
    }

    {
        let state_write = state.write().await;
        for imp in &imported {
            state_write.db.insert_package_with_names(&imp.pkg, &imp.pkg_names).await?;
        }
    }

    // Print summary
    if json_output {
        let packages: Vec<serde_json::Value> = imported.iter().map(|imp| {
            serde_json::json!({
                "pkgbase": imp.pkg.pkgbase,
                "version": imp.pkg.pkgver,
                "status": imp.pkg.status.to_string(),
                "install_reason": imp.pkg.install_reason.to_string(),
                "has_upstream": imp.pkg.has_upstream,
                "in_official_repos": imp.pkg.in_official_repos,
            })
        }).collect();

        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "status": "ok",
            "imported": packages.len(),
            "skipped_already_tracked": skipped_tracked,
            "explicit": explicit_count,
            "dependencies": dep_count,
            "updates_available": update_count,
            "no_upstream": no_upstream,
            "deprecated": deprecated_count,
            "local_only": local_only_count,
            "packages": packages,
        }))?);
    } else {
        println!("\n=== Import complete ===");
        println!("Imported: {} packages", imported.len());
        println!("  Explicit: {}", explicit_count);
        println!("  Dependencies: {}", dep_count);
        println!("  Updates available: {}", update_count);
        println!("  No AUR upstream: {}", no_upstream);
        if deprecated_count > 0 {
            println!("    - Deprecated (now in official): {}", deprecated_count);
        }
        if local_only_count > 0 {
            println!("    - Local only: {}", local_only_count);
        }
        println!("Skipped (already tracked): {}", skipped_tracked);

        // Show deprecated packages prominently
        let deprecated_pkgs: Vec<_> = imported.iter()
            .filter(|p| !p.pkg.has_upstream && p.pkg.in_official_repos)
            .collect();
        if !deprecated_pkgs.is_empty() {
            println!("\nDeprecated packages (consider removing - now in official repos):");
            for imp in deprecated_pkgs {
                println!("  {} {}", imp.pkg.pkgbase, imp.pkg.pkgver.as_deref().unwrap_or("-"));
            }
        }

        // Show local-only packages
        let local_only_pkgs: Vec<_> = imported.iter()
            .filter(|p| !p.pkg.has_upstream && !p.pkg.in_official_repos)
            .collect();
        if !local_only_pkgs.is_empty() && local_only_pkgs.len() <= 15 {
            println!("\nLocal-only packages (no AUR or official repo upstream):");
            for imp in local_only_pkgs {
                println!("  {} {}", imp.pkg.pkgbase, imp.pkg.pkgver.as_deref().unwrap_or("-"));
            }
        }
    }

    // Check if config should be updated
    if !json_output && !imported.is_empty() {
        let repo_dir = db_path.parent().unwrap_or(std::path::Path::new("."));

        // Extract repo name from database filename (e.g., "aur.db.tar.zst" -> "aur")
        let repo_name = db_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| {
                s.strip_suffix(".db.tar.zst")
                    .or_else(|| s.strip_suffix(".db.tar.gz"))
                    .or_else(|| s.strip_suffix(".db.tar.xz"))
                    .or_else(|| s.strip_suffix(".db.tar"))
                    .or_else(|| s.strip_suffix(".db"))
                    .unwrap_or(s)
            })
            .unwrap_or("aur");

        // Check if config differs
        let config_differs = {
            let state_read = state.read().await;
            state_read.config.repo.path != repo_dir || state_read.config.repo.name != repo_name
        };

        if config_differs {
            println!();
            println!("Your current config points to a different repository:");
            {
                let state_read = state.read().await;
                println!("  Current: {} ({})", state_read.config.repo.path.display(), state_read.config.repo.name);
            }
            println!("  Imported: {} ({})", repo_dir.display(), repo_name);
            println!();
            print!("Update config to use the imported repository? [Y/n] ");
            use std::io::Write;
            std::io::stdout().flush()?;

            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;

            if input.trim().is_empty() || input.trim().to_lowercase() == "y" {
                match config::update_config_repo(repo_dir, repo_name) {
                    Ok(path) => {
                        println!("Config updated: {}", path.display());
                        println!("Restart repoman for changes to take effect.");
                    }
                    Err(e) => {
                        println!("Failed to update config: {}", e);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Check if packages exist in official Arch repos using pacman
///
/// Returns a set of pkgnames that exist in official repos
fn check_official_repos(pkgnames: &[String]) -> std::collections::HashSet<String> {
    use std::collections::HashSet;

    let mut in_official: HashSet<String> = HashSet::new();

    // Use pacman -Ss to search for exact package names
    // We batch this for efficiency
    if pkgnames.is_empty() {
        return in_official;
    }

    // Check each package individually with pacman -Si (package info)
    // This is more accurate than -Ss which does substring matching
    for pkgname in pkgnames {
        let output = std::process::Command::new("pacman")
            .args(["-Si", pkgname])
            .output();

        if let Ok(output) = output {
            if output.status.success() {
                // Package found in sync databases (official repos)
                in_official.insert(pkgname.clone());
            }
        }
    }

    in_official
}

/// Infer install reasons and check official repos
///
/// Uses multiple heuristics:
/// 1. Packages depended upon by others in the set -> Dependency
/// 2. Compat packages (-compat-libs, -shim, version suffixes like icu70) without upstream -> Dependency
/// 3. Packages without upstream that exist in official repos -> mark in_official_repos
/// 4. Everything else -> Explicit (conservative default)
fn infer_install_reasons(packages: &mut [ImportedPackage]) {
    use std::collections::{HashMap, HashSet};
    use repo::db_parser::strip_version_constraint;

    // Collect all pkgnames for packages without upstream to check official repos
    let pkgnames_to_check: Vec<String> = packages
        .iter()
        .filter(|imp| !imp.pkg.has_upstream)
        .flat_map(|imp| imp.pkg_names.iter().map(|n| n.pkgname.clone()))
        .collect();

    // Check which packages are in official repos
    let in_official = check_official_repos(&pkgnames_to_check);

    // Mark packages as in_official_repos if any of their pkgnames are in official repos
    for imp in packages.iter_mut() {
        if !imp.pkg.has_upstream {
            let is_in_official = imp.pkg_names.iter().any(|n| in_official.contains(&n.pkgname));
            imp.pkg.in_official_repos = is_in_official;
        }
    }

    // Build pkgname -> pkgbase map (including provides)
    let mut pkgname_to_pkgbase: HashMap<String, String> = HashMap::new();
    for imp in packages.iter() {
        for entry in &imp.pkg_entries {
            pkgname_to_pkgbase.insert(entry.name.clone(), imp.pkg.pkgbase.clone());
        }
        for name in &imp.pkg_names {
            pkgname_to_pkgbase.insert(name.pkgname.clone(), imp.pkg.pkgbase.clone());
        }
        // Also add provides
        for entry in &imp.pkg_entries {
            for provided in &entry.provides {
                let provided_name = strip_version_constraint(provided);
                pkgname_to_pkgbase.insert(provided_name.to_string(), imp.pkg.pkgbase.clone());
            }
        }
    }

    // Build reverse deps: pkgbase -> set of pkgbases that depend on it
    let mut reverse_deps: HashMap<String, HashSet<String>> = HashMap::new();

    for imp in packages.iter() {
        for entry in &imp.pkg_entries {
            for dep in &entry.depends {
                let dep_name = strip_version_constraint(dep);
                if let Some(dep_pkgbase) = pkgname_to_pkgbase.get(dep_name) {
                    // Skip self-dependencies
                    if dep_pkgbase != &imp.pkg.pkgbase {
                        reverse_deps
                            .entry(dep_pkgbase.clone())
                            .or_default()
                            .insert(imp.pkg.pkgbase.clone());
                    }
                }
            }
        }
    }

    // Determine install reason using multiple heuristics
    for imp in packages.iter_mut() {
        let has_dependents = reverse_deps
            .get(&imp.pkg.pkgbase)
            .map(|d| !d.is_empty())
            .unwrap_or(false);

        // Heuristic: packages with dependents within the repo are dependencies
        if has_dependents {
            imp.pkg.install_reason = models::InstallReason::Dependency;
            continue;
        }

        // Heuristic: version compatibility packages (icu70, ncurses5-compat-libs, etc.)
        let pkgbase = &imp.pkg.pkgbase;
        let looks_like_compat = pkgbase.ends_with("-compat-libs")
            || pkgbase.ends_with("-shim")
            || pkgbase.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false)
               && pkgbase.chars().filter(|c| c.is_ascii_digit()).count() >= 2;

        // If it looks like a compat package AND doesn't have AUR upstream, likely a dep
        if looks_like_compat && !imp.pkg.has_upstream {
            imp.pkg.install_reason = models::InstallReason::Dependency;
        } else {
            // Default to explicit - user probably wanted this package
            imp.pkg.install_reason = models::InstallReason::Explicit;
        }
    }
}
