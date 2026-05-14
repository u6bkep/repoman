mod api;
mod assets;
mod aur;
mod build;
mod config;
mod db;
mod models;
mod repo;
mod service;
mod state;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, broadcast};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::aur::AurClient;
use crate::build::BuildSystem;
use crate::config::Config;
use crate::db::Database;
use crate::models::InstallReason;
use crate::repo::RepoManager;
use crate::service::status::{DerivedStatus, StatusInputs, derive_status, is_orphan};
use crate::state::{RepoState, SrcInfoCache};

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

// ==================== Enriched package for CLI display ====================

struct EnrichedPackage {
    pkg: models::Package,
    status: DerivedStatus,
    pkgver: Option<String>,
    current_commit: Option<String>,
}

async fn enrich_package(
    pkg: &models::Package,
    db: &Database,
    srcinfo_cache: &SrcInfoCache,
    repo_state: &RepoState,
    active_builds: &HashSet<String>,
    aur_clones: &Path,
) -> EnrichedPackage {
    let clone_dir = pkg.clone_path(aur_clones);
    let head_commit = SrcInfoCache::get_head_commit(&clone_dir).ok().flatten();
    let srcinfo = srcinfo_cache.get(&pkg.pkgbase);
    let pkgver = srcinfo.map(|s| s.srcinfo.full_version());

    let latest_build = if let Some(ref commit) = pkg.approved_commit {
        db.get_latest_build_for_commit(&pkg.pkgbase, commit)
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    let status = derive_status(&StatusInputs {
        approved_commit: pkg.approved_commit.as_deref(),
        head_commit: head_commit.as_deref(),
        latest_build: latest_build.as_ref(),
        is_active_build: active_builds.contains(&pkg.pkgbase),
        in_repo: repo_state.has_pkgbase(&pkg.pkgbase),
    });

    EnrichedPackage {
        pkg: pkg.clone(),
        status,
        pkgver,
        current_commit: head_commit,
    }
}

/// Resolve a pkgname or pkgbase to a pkgbase using the SrcInfoCache
fn resolve_pkgbase_from_cache(cache: &SrcInfoCache, name: &str) -> Option<String> {
    // First check if it's a pkgbase directly
    if cache.get(name).is_some() {
        return Some(name.to_string());
    }
    // Search all cached entries for a matching pkgname
    for (pkgbase, cached) in cache.iter() {
        for pkg in &cached.srcinfo.packages {
            if pkg.pkgname == name {
                return Some(pkgbase.to_string());
            }
        }
    }
    None
}

/// Find orphaned packages using SrcInfoCache
fn find_orphans(packages: &[models::Package], cache: &SrcInfoCache) -> Vec<String> {
    use crate::repo::db_parser::strip_version_constraint;

    // Collect all deps of explicit packages
    let mut explicit_deps = HashSet::new();
    for pkg in packages {
        if pkg.install_reason == InstallReason::Explicit
            && let Some(cached) = cache.get(&pkg.pkgbase)
        {
            for dep in &cached.srcinfo.depends {
                explicit_deps.insert(strip_version_constraint(dep).to_string());
            }
            for p in &cached.srcinfo.packages {
                for dep in &p.depends {
                    explicit_deps.insert(strip_version_constraint(dep).to_string());
                }
            }
        }
    }

    // Find orphans
    packages
        .iter()
        .filter(|p| p.install_reason == InstallReason::Dependency)
        .filter(|p| {
            let pkgnames: Vec<String> = cache
                .get(&p.pkgbase)
                .map(|c| {
                    c.srcinfo
                        .packages
                        .iter()
                        .map(|p| p.pkgname.clone())
                        .collect()
                })
                .unwrap_or_default();
            is_orphan(&pkgnames, &explicit_deps)
        })
        .map(|p| p.pkgbase.clone())
        .collect()
}

/// Parse a status string into DerivedStatus
fn parse_status_filter(s: &str) -> Result<DerivedStatus> {
    match s {
        "pending_review" => Ok(DerivedStatus::PendingReview),
        "approved" => Ok(DerivedStatus::Approved),
        "building" => Ok(DerivedStatus::Building),
        "failed" => Ok(DerivedStatus::Failed),
        "up_to_date" => Ok(DerivedStatus::UpToDate),
        "update_available" => Ok(DerivedStatus::UpdateAvailable),
        "missing" => Ok(DerivedStatus::Missing),
        _ => anyhow::bail!("Invalid status: {}", s),
    }
}

// ==================== CLI State ====================

/// State shared across CLI commands (not the web server AppState)
struct CliState {
    db: Database,
    config: Config,
    aur: AurClient,
    build_system: BuildSystem,
    repo: RepoManager,
    srcinfo_cache: SrcInfoCache,
    repo_state: RepoState,
}

impl CliState {
    /// Resolve a name (pkgname or pkgbase) to a package
    async fn resolve_package(&self, name: &str) -> Result<Option<models::Package>> {
        // Try direct pkgbase lookup first
        if let Some(pkg) = self.db.get_package(name).await? {
            return Ok(Some(pkg));
        }
        // Try resolving via SrcInfoCache
        if let Some(pkgbase) = resolve_pkgbase_from_cache(&self.srcinfo_cache, name) {
            return self.db.get_package(&pkgbase).await;
        }
        Ok(None)
    }

    /// Get pkgnames for a pkgbase from the SrcInfoCache
    fn get_pkgnames(&self, pkgbase: &str) -> Vec<String> {
        self.srcinfo_cache
            .get(pkgbase)
            .map(|c| {
                c.srcinfo
                    .packages
                    .iter()
                    .map(|p| p.pkgname.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Enrich a package with derived status and metadata
    async fn enrich(&self, pkg: &models::Package) -> EnrichedPackage {
        let active_builds: HashSet<String> = HashSet::new();
        enrich_package(
            pkg,
            &self.db,
            &self.srcinfo_cache,
            &self.repo_state,
            &active_builds,
            &self.config.resolved_cache().aur_clones,
        )
        .await
    }
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

    let resolved_cache = config.resolved_cache();

    // Initialize SrcInfoCache
    let mut srcinfo_cache = SrcInfoCache::new();
    let packages = db.list_packages().await?;
    let pkgbases: Vec<String> = packages.iter().map(|p| p.pkgbase.clone()).collect();
    srcinfo_cache.bulk_load(&resolved_cache.aur_clones, &pkgbases)?;

    // Initialize RepoState
    let repo_state = RepoState::load(&config.repo.path, &config.repo.name)?;

    let json_output = cli.json;

    // For the serve command, we need the full AppState with broadcast channels etc.
    if matches!(cli.command, Some(Commands::Serve) | None) {
        return run_server(db, config, srcinfo_cache, repo_state).await;
    }

    // Build CLI state for non-serve commands
    let mut cli_state = CliState {
        db,
        config: config.clone(),
        aur: AurClient::new(),
        build_system: BuildSystem::new(config.clone()),
        repo: RepoManager::new(config.clone()),
        srcinfo_cache,
        repo_state,
    };

    match cli.command {
        Some(Commands::Add {
            name,
            asdeps,
            with_deps,
        }) => {
            cmd_add(&mut cli_state, &name, asdeps, with_deps, json_output).await?;
        }
        Some(Commands::Remove {
            name,
            delete_files,
            delete_clone,
            recursive,
        }) => {
            cmd_remove(&mut cli_state, &name, delete_files, delete_clone, recursive).await?;
        }
        Some(Commands::List {
            status,
            explicit,
            deps,
            orphans,
        }) => {
            cmd_list(&cli_state, status, explicit, deps, orphans, json_output).await?;
        }
        Some(Commands::Info { name }) => {
            cmd_info(&cli_state, &name, json_output).await?;
        }
        Some(Commands::Diff { name }) => {
            cmd_diff(&cli_state, &name).await?;
        }
        Some(Commands::Review { name, stdout }) => {
            cmd_review(&cli_state, &name, stdout).await?;
        }
        Some(Commands::Approve { name }) => {
            cmd_approve(&mut cli_state, &name).await?;
        }
        Some(Commands::Build {
            names,
            all,
            updates,
        }) => {
            cmd_build(&mut cli_state, names, all, updates).await?;
        }
        Some(Commands::Logs { id, follow, lines }) => {
            cmd_logs(&cli_state, &id, follow, lines).await?;
        }
        Some(Commands::CheckUpdates) => {
            cmd_check_updates(&mut cli_state, json_output).await?;
        }
        Some(Commands::RebuildImage) => {
            cmd_rebuild_image(&cli_state).await?;
        }
        Some(Commands::Status) => {
            cmd_status(&cli_state, json_output).await?;
        }
        Some(Commands::Mark {
            name,
            explicit,
            asdeps,
        }) => {
            cmd_mark(&cli_state, &name, explicit, asdeps).await?;
        }
        Some(Commands::Config { .. }) => {
            unreachable!("Config command handled earlier");
        }
        Some(Commands::Import { db_path, dry_run }) => {
            cmd_import(&mut cli_state, &db_path, dry_run, json_output).await?;
        }
        Some(Commands::Serve) | None => {
            unreachable!("Serve command handled earlier");
        }
    }

    Ok(())
}

async fn run_server(
    db: Database,
    config: Config,
    srcinfo_cache: SrcInfoCache,
    repo_state: RepoState,
) -> Result<()> {
    // Check podman availability
    BuildSystem::check_podman()?;

    // Recover stale builds left in pending/running state (e.g. after server crash)
    match db.recover_stale_builds().await {
        Ok(0) => {}
        Ok(n) => tracing::info!("Recovered {} stale build(s) from previous run", n),
        Err(e) => tracing::error!("Failed to recover stale builds: {}", e),
    }

    let (tx, _) = broadcast::channel(100);

    let state: api::SharedState = Arc::new(api::AppState {
        db,
        config: config.clone(),
        aur: AurClient::new(),
        build: BuildSystem::new(config.clone()),
        repo: RepoManager::new(config.clone()),
        srcinfo_cache: RwLock::new(srcinfo_cache),
        repo_state: RwLock::new(repo_state),
        build_stages: RwLock::new(HashMap::new()),
        repo_mutex: Mutex::new(()),
        events: tx,
        active_builds: RwLock::new(HashSet::new()),
        build_cancel_flags: RwLock::new(HashMap::new()),
    });

    let app = api::router(state).fallback(assets::static_handler);

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
    install_reason: InstallReason,
}

async fn cmd_add(
    state: &mut CliState,
    name: &str,
    asdeps: bool,
    with_deps: bool,
    json_output: bool,
) -> Result<()> {
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

    // Check if already exists
    if let Some(pkg) = state.db.get_package(name).await? {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "status": "already_tracked",
                    "pkgbase": pkg.pkgbase
                })
            );
        } else {
            println!("Package {} already tracked", pkg.pkgbase);
        }
        return Ok(());
    }

    if let Some(pkgbase) = resolve_pkgbase_from_cache(&state.srcinfo_cache, name)
        && state.db.get_package(&pkgbase).await?.is_some()
    {
        if json_output {
            println!(
                "{}",
                serde_json::json!({
                    "status": "already_tracked",
                    "pkgbase": pkgbase
                })
            );
        } else {
            println!("Package {} already tracked (as {})", name, pkgbase);
        }
        return Ok(());
    }

    // Add the main package
    let install_reason = if asdeps {
        InstallReason::Dependency
    } else {
        InstallReason::Explicit
    };

    add_package_internal(
        state,
        name,
        install_reason,
        &mut added_packages,
        json_output,
    )
    .await?;

    // If --with-deps, recursively add missing AUR dependencies
    if with_deps {
        // Check if base image exists (needed for dep detection)
        let image_exists = state.build_system.image_exists().unwrap_or(false);
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
            let mut processed: HashSet<String> = HashSet::new();
            for p in &added_packages {
                processed.insert(p.pkgbase.clone());
            }

            // Queue of packages to check for deps
            let mut to_check: Vec<String> =
                added_packages.iter().map(|p| p.pkgbase.clone()).collect();

            while let Some(pkgbase) = to_check.pop() {
                // Get the srcinfo for this package
                let clone_path = state.config.resolved_cache().aur_clones.join(&pkgbase);
                let srcinfo = match aur::read_srcinfo(&clone_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                // Get currently tracked package names from cache + DB
                let tracked_packages = state.db.list_packages().await?;
                let mut tracked_pkgnames: HashSet<String> = HashSet::new();
                for p in &tracked_packages {
                    tracked_pkgnames.insert(p.pkgbase.clone());
                    let pkgnames = state.get_pkgnames(&p.pkgbase);
                    for n in pkgnames {
                        tracked_pkgnames.insert(n);
                    }
                }

                // Find missing AUR deps
                let container_image = &state.config.container.image;
                let repo_name = &state.config.repo.name;
                let repo_path = &state.config.repo.path;
                let missing = state
                    .aur
                    .find_missing_aur_deps(
                        &srcinfo,
                        &tracked_pkgnames,
                        container_image,
                        repo_name,
                        repo_path,
                    )
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
                        state,
                        &dep.name,
                        InstallReason::Dependency,
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

    // Print summary
    if json_output {
        let added: Vec<serde_json::Value> = added_packages
            .iter()
            .map(|pkg| {
                serde_json::json!({
                    "pkgbase": pkg.pkgbase,
                    "version": pkg.version,
                    "install_reason": match pkg.install_reason {
                        InstallReason::Explicit => "explicit",
                        InstallReason::Dependency => "dependency",
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

        println!(
            "{}",
            serde_json::json!({
                "status": "ok",
                "added": added,
                "failed_deps": failed
            })
        );
    } else {
        println!();
        if added_packages.is_empty() {
            println!("No packages added.");
        } else {
            println!("Added {} package(s):", added_packages.len());
            for pkg in &added_packages {
                let reason_str = match pkg.install_reason {
                    InstallReason::Explicit => "explicit",
                    InstallReason::Dependency => "dependency",
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
    state: &mut CliState,
    name: &str,
    install_reason: InstallReason,
    added_packages: &mut Vec<AddedPackage>,
    json_output: bool,
) -> Result<()> {
    // Check if already exists
    if state.db.get_package(name).await?.is_some() {
        return Ok(()); // Already tracked
    }
    if let Some(pkgbase) = resolve_pkgbase_from_cache(&state.srcinfo_cache, name)
        && state.db.get_package(&pkgbase).await?.is_some()
    {
        return Ok(()); // Already tracked
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
            InstallReason::Explicit => "explicit",
            InstallReason::Dependency => "dependency",
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
    let version = srcinfo.full_version();

    // Create package record (V2 -- no status, pkgver, current_commit fields)
    let now = chrono::Utc::now();
    let pkg = models::Package {
        pkgbase: pkgbase.clone(),
        approved_commit: None,
        install_reason,
        auto_approve: false,
        added_at: now,
        updated_at: now,
    };

    // Insert package (no names table in V2)
    state.db.insert_package(&pkg).await?;

    // Add to SrcInfoCache
    state
        .srcinfo_cache
        .insert(pkgbase.clone(), srcinfo, current_commit);

    added_packages.push(AddedPackage {
        pkgbase: pkgbase.clone(),
        version,
        install_reason,
    });

    Ok(())
}

async fn cmd_remove(
    state: &mut CliState,
    name: &str,
    delete_files: bool,
    delete_clone: bool,
    recursive: bool,
) -> Result<()> {
    let pkg = state
        .resolve_package(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    if delete_files {
        let pkgnames = state.get_pkgnames(&pkg.pkgbase);
        let pkgname_refs: Vec<&str> = pkgnames.iter().map(|n| n.as_str()).collect();
        println!("Removing from repo: {:?}", pkgname_refs);
        state.build_system.repo_remove(&pkgname_refs)?;

        for pkgname in &pkgnames {
            state.repo.delete_package_files(pkgname)?;
        }
    }

    let aur_clones = state.config.resolved_cache().aur_clones;

    if delete_clone {
        let clone_path = pkg.clone_path(&aur_clones);
        if clone_path.exists() {
            println!("Deleting clone: {}", clone_path.display());
            std::fs::remove_dir_all(&clone_path)?;
        }
        // Also clean up cached sources
        state.build_system.cleanup_all_sources(&pkg.pkgbase)?;
    }

    // Remove from SrcInfoCache
    state.srcinfo_cache.remove(&pkg.pkgbase);

    state.db.delete_package(&pkg.pkgbase).await?;
    println!("Removed {}", pkg.pkgbase);

    // If recursive, also remove orphaned dependencies
    if recursive {
        let all_packages = state.db.list_packages().await?;
        let orphan_pkgbases = find_orphans(&all_packages, &state.srcinfo_cache);
        if !orphan_pkgbases.is_empty() {
            println!("\nRemoving orphaned dependencies:");
            for orphan_pkgbase in orphan_pkgbases {
                if delete_files {
                    let pkgnames = state.get_pkgnames(&orphan_pkgbase);
                    let pkgname_refs: Vec<&str> = pkgnames.iter().map(|n| n.as_str()).collect();
                    state.build_system.repo_remove(&pkgname_refs)?;
                    for pkgname in &pkgnames {
                        state.repo.delete_package_files(pkgname)?;
                    }
                }
                if delete_clone
                    && let Some(orphan_pkg) = state.db.get_package(&orphan_pkgbase).await?
                {
                    let clone_path = orphan_pkg.clone_path(&aur_clones);
                    if clone_path.exists() {
                        std::fs::remove_dir_all(&clone_path)?;
                    }
                    // Also clean up cached sources
                    state.build_system.cleanup_all_sources(&orphan_pkgbase)?;
                }
                state.srcinfo_cache.remove(&orphan_pkgbase);
                state.db.delete_package(&orphan_pkgbase).await?;
                println!("  Removed {} (orphan)", orphan_pkgbase);
            }
        }
    }

    Ok(())
}

async fn cmd_list(
    state: &CliState,
    status: Option<String>,
    explicit: bool,
    deps: bool,
    orphans_flag: bool,
    json_output: bool,
) -> Result<()> {
    let active_builds: HashSet<String> = HashSet::new();
    let aur_clones = state.config.resolved_cache().aur_clones;

    // Get all packages first, then filter
    let all_packages = if explicit {
        state
            .db
            .list_packages_by_reason(InstallReason::Explicit)
            .await?
    } else if deps {
        state
            .db
            .list_packages_by_reason(InstallReason::Dependency)
            .await?
    } else {
        state.db.list_packages().await?
    };

    // Handle orphans filter
    if orphans_flag {
        let all_for_orphan_check = state.db.list_packages().await?;
        let orphan_pkgbases = find_orphans(&all_for_orphan_check, &state.srcinfo_cache);
        let orphan_set: HashSet<&str> = orphan_pkgbases.iter().map(|s| s.as_str()).collect();

        let orphan_packages: Vec<&models::Package> = all_for_orphan_check
            .iter()
            .filter(|p| orphan_set.contains(p.pkgbase.as_str()))
            .collect();

        let mut enriched: Vec<EnrichedPackage> = Vec::new();
        for pkg in &orphan_packages {
            enriched.push(
                enrich_package(
                    pkg,
                    &state.db,
                    &state.srcinfo_cache,
                    &state.repo_state,
                    &active_builds,
                    &aur_clones,
                )
                .await,
            );
        }

        print_package_list(&enriched, json_output, true)?;
        return Ok(());
    }

    // Enrich all packages
    let mut enriched: Vec<EnrichedPackage> = Vec::new();
    for pkg in &all_packages {
        enriched.push(
            enrich_package(
                pkg,
                &state.db,
                &state.srcinfo_cache,
                &state.repo_state,
                &active_builds,
                &aur_clones,
            )
            .await,
        );
    }

    // Filter by status if specified
    if let Some(ref s) = status {
        let filter_status = parse_status_filter(s)?;
        enriched.retain(|e| e.status == filter_status);
    }

    print_package_list(&enriched, json_output, false)?;

    Ok(())
}

fn print_package_list(
    packages: &[EnrichedPackage],
    json_output: bool,
    is_orphan_list: bool,
) -> Result<()> {
    if json_output {
        let json_packages: Vec<serde_json::Value> = packages
            .iter()
            .map(|ep| {
                serde_json::json!({
                    "pkgbase": ep.pkg.pkgbase,
                    "version": ep.pkgver,
                    "status": ep.status.to_string(),
                    "install_reason": ep.pkg.install_reason.to_string(),
                    "auto_approve": ep.pkg.auto_approve,
                    "current_commit": ep.current_commit,
                    "approved_commit": ep.pkg.approved_commit,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_packages)?);
        return Ok(());
    }

    if packages.is_empty() {
        if is_orphan_list {
            println!("No orphaned packages found");
        } else {
            println!("No packages found");
        }
        return Ok(());
    }

    println!(
        "{:<30} {:<20} {:<15} {:<10}",
        "PKGBASE", "VERSION", "STATUS", "REASON"
    );
    println!("{}", "-".repeat(75));

    for ep in packages {
        println!(
            "{:<30} {:<20} {:<15} {:<10}",
            ep.pkg.pkgbase,
            ep.pkgver.as_deref().unwrap_or("-"),
            ep.status,
            ep.pkg.install_reason
        );
    }

    println!("\nTotal: {} package(s)", packages.len());

    Ok(())
}

async fn cmd_info(state: &CliState, name: &str, json_output: bool) -> Result<()> {
    let pkg = state
        .resolve_package(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let enriched = state.enrich(&pkg).await;

    let srcinfo = state.srcinfo_cache.get(&pkg.pkgbase);
    let aur_clones = state.config.resolved_cache().aur_clones;

    if json_output {
        let pkgnames: Vec<serde_json::Value> = srcinfo
            .map(|c| {
                c.srcinfo
                    .packages
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "pkgname": p.pkgname,
                            "pkgdesc": p.pkgdesc.as_ref().or(c.srcinfo.pkgdesc.as_ref()),
                            "depends": p.depends,
                            "provides": p.provides,
                            "conflicts": p.conflicts,
                            "repo_version": state.repo_state.get_version_for_pkgname(&p.pkgname),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "pkgbase": pkg.pkgbase,
                "version": enriched.pkgver,
                "status": enriched.status.to_string(),
                "install_reason": pkg.install_reason.to_string(),
                "auto_approve": pkg.auto_approve,
                "aur_url": pkg.aur_url(),
                "clone_path": pkg.clone_path(&aur_clones),
                "current_commit": enriched.current_commit,
                "approved_commit": pkg.approved_commit,
                "added_at": pkg.added_at.to_rfc3339(),
                "updated_at": pkg.updated_at.to_rfc3339(),
                "packages": pkgnames,
            }))?
        );
        return Ok(());
    }

    println!("Package: {}", pkg.pkgbase);
    println!("Version: {}", enriched.pkgver.as_deref().unwrap_or("-"));
    println!("Status: {}", enriched.status);
    println!("Install reason: {}", pkg.install_reason);
    println!("Auto-approve: {}", pkg.auto_approve);
    println!("URL: {}", pkg.aur_url());
    println!("Clone path: {}", pkg.clone_path(&aur_clones).display());
    println!(
        "Current commit: {}",
        enriched.current_commit.as_deref().unwrap_or("-")
    );
    println!(
        "Approved commit: {}",
        pkg.approved_commit.as_deref().unwrap_or("-")
    );
    println!("Added: {}", pkg.added_at);
    println!("Updated: {}", pkg.updated_at);
    println!();
    println!("Packages produced:");
    if let Some(cached) = srcinfo {
        for sp in &cached.srcinfo.packages {
            println!(
                "  - {} ({})",
                sp.pkgname,
                sp.pkgdesc
                    .as_deref()
                    .or(cached.srcinfo.pkgdesc.as_deref())
                    .unwrap_or("-")
            );
        }
    } else {
        println!("  (no .SRCINFO cached)");
    }

    Ok(())
}

async fn cmd_diff(state: &CliState, name: &str) -> Result<()> {
    let pkg = state
        .resolve_package(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    aur::AurGit::ensure_cloned(&pkg.pkgbase, &clone_path)?;

    if let Some(ref approved) = pkg.approved_commit {
        let current =
            SrcInfoCache::get_head_commit(&clone_path)?.unwrap_or_else(|| "HEAD".to_string());
        let diff = aur::AurGit::diff(&clone_path, approved, &current)?;

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

async fn cmd_review(state: &CliState, name: &str, stdout: bool) -> Result<()> {
    let pkg = state
        .resolve_package(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    aur::AurGit::ensure_cloned(&pkg.pkgbase, &clone_path)?;
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

        let enriched = state.enrich(&pkg).await;
        println!("\nReviewed: {}", pkg.pkgbase);
        println!("Status: {}", enriched.status);
        if enriched.status == DerivedStatus::PendingReview
            || enriched.status == DerivedStatus::UpdateAvailable
        {
            println!("\nTo approve: repoman approve {}", pkg.pkgbase);
        }
    }

    Ok(())
}

async fn cmd_approve(state: &mut CliState, name: &str) -> Result<()> {
    let pkg = state
        .resolve_package(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let clone_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    aur::AurGit::ensure_cloned(&pkg.pkgbase, &clone_path)?;

    let current_commit = SrcInfoCache::get_head_commit(&clone_path)?
        .ok_or_else(|| anyhow::anyhow!("Could not determine HEAD commit"))?;

    state
        .db
        .approve_package(&pkg.pkgbase, &current_commit, false)
        .await?;
    println!("Approved {} at commit {}", pkg.pkgbase, current_commit);
    println!("Run 'repoman build {}' to build", pkg.pkgbase);

    Ok(())
}

async fn cmd_build(
    state: &mut CliState,
    names: Vec<String>,
    all: bool,
    updates: bool,
) -> Result<()> {
    let active_builds: HashSet<String> = HashSet::new();
    let aur_clones = state.config.resolved_cache().aur_clones.clone();

    // Determine which packages to build
    let packages_to_build: Vec<String> = if all {
        // Build all approved packages
        let all_packages = state.db.list_packages().await?;
        let mut buildable = Vec::new();
        for pkg in &all_packages {
            let enriched = enrich_package(
                pkg,
                &state.db,
                &state.srcinfo_cache,
                &state.repo_state,
                &active_builds,
                &aur_clones,
            )
            .await;
            if enriched.status == DerivedStatus::Approved
                || enriched.status == DerivedStatus::UpdateAvailable
            {
                buildable.push(pkg.pkgbase.clone());
            }
        }
        buildable
    } else if updates {
        // Build packages with updates
        let all_packages = state.db.list_packages().await?;
        let mut buildable = Vec::new();
        for pkg in &all_packages {
            let enriched = enrich_package(
                pkg,
                &state.db,
                &state.srcinfo_cache,
                &state.repo_state,
                &active_builds,
                &aur_clones,
            )
            .await;
            if enriched.status == DerivedStatus::UpdateAvailable {
                buildable.push(pkg.pkgbase.clone());
            }
        }
        buildable
    } else {
        // Resolve provided names to pkgbases
        let mut resolved = Vec::new();
        for name in &names {
            if let Some(pkg) = state.resolve_package(name).await? {
                resolved.push(pkg.pkgbase);
            } else {
                println!("Warning: package '{}' not found, skipping", name);
            }
        }
        resolved
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
    // Check base image exists
    if !state.build_system.image_exists()? {
        anyhow::bail!("Base image not found. Run 'repoman rebuild-image' first.");
    }

    // Get packages and validate
    let mut valid_packages: Vec<models::Package> = Vec::new();
    for pkgbase in &packages_to_build {
        if let Some(pkg) = state.db.get_package(pkgbase).await? {
            let enriched = enrich_package(
                &pkg,
                &state.db,
                &state.srcinfo_cache,
                &state.repo_state,
                &active_builds,
                &aur_clones,
            )
            .await;
            if enriched.status == DerivedStatus::Approved
                || enriched.status == DerivedStatus::UpdateAvailable
            {
                valid_packages.push(pkg);
            } else {
                println!(
                    "Skipping {}: status is '{}' (must be approved)",
                    pkgbase, enriched.status
                );
            }
        }
    }

    if valid_packages.is_empty() {
        println!("No packages ready to build");
        return Ok(());
    }

    // Build pkgname -> pkgbase mapping from SrcInfoCache
    let pkgbases_set: HashSet<String> = valid_packages.iter().map(|p| p.pkgbase.clone()).collect();
    let mut pkgname_to_pkgbase: HashMap<String, String> = HashMap::new();
    for pkg in &valid_packages {
        let pkgnames = state.get_pkgnames(&pkg.pkgbase);
        for pkgname in pkgnames {
            pkgname_to_pkgbase.insert(pkgname, pkg.pkgbase.clone());
        }
    }

    // Build dependency graph from SrcInfoCache
    let mut deps_graph: HashMap<String, Vec<String>> = HashMap::new();
    for pkg in &valid_packages {
        let mut pkg_deps: Vec<String> = Vec::new();
        if let Some(cached) = state.srcinfo_cache.get(&pkg.pkgbase) {
            // Global deps
            for dep in &cached.srcinfo.depends {
                let dep_name = aur::DepResolver::dep_name(dep);
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
                    let dep_name = aur::DepResolver::dep_name(dep);
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

    // Topological sort
    let build_order = topological_sort_cli(&pkgbases_set, &deps_graph)?;

    println!("\nBuild order (dependencies first):");
    for (i, pkgbase) in build_order.iter().enumerate() {
        println!("  {}. {}", i + 1, pkgbase);
    }
    println!();

    // Run builds sequentially
    let mut failed: HashSet<String> = HashSet::new();
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

        match cmd_build_single(state, pkgbase).await {
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
    nodes: &HashSet<String>,
    deps: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>> {
    use std::collections::VecDeque;

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
            if let Some(other_deps) = deps.get(other)
                && other_deps.contains(&node)
                && !processed.contains(other)
            {
                let degree = in_degree.get_mut(other).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    queue.push_back(other.clone());
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
async fn cmd_build_single(state: &mut CliState, name: &str) -> Result<()> {
    let pkg = state
        .resolve_package(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    // Derive status to check if package is buildable
    let enriched = state.enrich(&pkg).await;
    if enriched.status != DerivedStatus::Approved
        && enriched.status != DerivedStatus::UpdateAvailable
    {
        anyhow::bail!(
            "Package must be approved before building (current status: {})",
            enriched.status
        );
    }

    if !state.build_system.image_exists()? {
        anyhow::bail!("Base image not found. Run 'repoman rebuild-image' first.");
    }

    let log_path = state
        .config
        .resolved_cache()
        .logs
        .join(format!("{}-build.log", pkg.pkgbase));
    let build_system = BuildSystem::new(state.config.clone());

    std::fs::create_dir_all(&state.config.repo.path)?;
    build_system.init_repo()?;

    // No status update needed in V2 -- status is derived

    println!("Building {}...", pkg.pkgbase);

    let source_path = pkg.clone_path(&state.config.resolved_cache().aur_clones);
    // Ensure clone exists (re-clone from AUR if missing)
    aur::AurGit::ensure_cloned(&pkg.pkgbase, &source_path)?;
    let result = build_system.run_build_sync(&pkg.pkgbase, &source_path, &log_path);

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
                    // Reload RepoState after repo-add
                    state.repo_state.reload()?;

                    println!("\nBuild successful!");
                    println!("Packages added to repository:");
                    for pkg_path in &repo_packages {
                        println!(
                            "  - {}",
                            pkg_path.file_name().unwrap_or_default().to_string_lossy()
                        );
                    }
                    Ok(())
                }
                Err(e) => {
                    // No status update needed in V2 -- status will derive as Failed or Approved
                    Err(e)
                }
            }
        }
        Err(e) => {
            // No status update needed in V2
            println!("\nBuild failed: {}", e);
            println!("Check logs at: {}", log_path.display());
            anyhow::bail!("Build failed");
        }
    }
}

async fn cmd_logs(state: &CliState, id: &str, follow: bool, lines: Option<usize>) -> Result<()> {
    // Try to find the log path
    let logs_dir = state.config.resolved_cache().logs;
    let log_path: PathBuf = if let Some(build) = state.db.get_build(id).await? {
        // Found a build by ID
        build
            .resolve_log_path(&logs_dir)
            .ok_or_else(|| anyhow::anyhow!("Build {} has no log file", id))?
    } else {
        // Try to interpret as package name and get latest build
        let pkgbase: Option<String> = if let Some(pkg) = state.resolve_package(id).await? {
            Some(pkg.pkgbase)
        } else {
            None
        };

        if let Some(pkgbase) = pkgbase {
            // Get the latest build for this package
            let builds = state.db.list_builds(&pkgbase, 1, 0).await?;
            if let Some(build) = builds.first() {
                build
                    .resolve_log_path(&logs_dir)
                    .unwrap_or_else(|| logs_dir.join(format!("{}-build.log", pkgbase)))
            } else {
                // No builds yet, try the default log path
                logs_dir.join(format!("{}-build.log", pkgbase))
            }
        } else {
            // Try direct log file path as fallback
            let direct_log = logs_dir.join(format!("{}-build.log", id));
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

async fn cmd_check_updates(state: &mut CliState, json_output: bool) -> Result<()> {
    let packages = state.db.list_packages().await?;
    let mut updates: Vec<serde_json::Value> = Vec::new();
    let active_builds: HashSet<String> = HashSet::new();
    let aur_clones = state.config.resolved_cache().aur_clones.clone();

    for pkg in packages {
        // Derive status to check if building
        let enriched = enrich_package(
            &pkg,
            &state.db,
            &state.srcinfo_cache,
            &state.repo_state,
            &active_builds,
            &aur_clones,
        )
        .await;

        if enriched.status == DerivedStatus::Building {
            if !json_output {
                println!("Skipping {} (build in progress)", pkg.pkgbase);
            }
            continue;
        }

        let clone_path = pkg.clone_path(&aur_clones);
        aur::AurGit::ensure_cloned(&pkg.pkgbase, &clone_path)?;

        if !json_output {
            print!("Checking {}... ", pkg.pkgbase);
        }

        if aur::AurGit::has_updates(&clone_path)? {
            // Read old SRCINFO before resetting to remote
            let old_srcinfo = aur::read_srcinfo(&clone_path).ok();
            let old_version = old_srcinfo.as_ref().map(|s| s.full_version());

            aur::AurGit::reset_to_remote(&clone_path)?;
            let new_commit = aur::AurGit::head_commit(&clone_path)?;

            let srcinfo = aur::read_srcinfo(&clone_path)?;
            let pkgver = srcinfo.full_version();

            // Clean up stale sources if version changed
            if old_version.as_deref() != Some(&pkgver) {
                let deleted = state.build_system.cleanup_stale_sources(
                    &pkg.pkgbase,
                    old_srcinfo.as_ref(),
                    &srcinfo,
                )?;
                if !deleted.is_empty() && !json_output {
                    println!("Cleaned {} stale source(s)", deleted.len());
                    print!("  ");
                }
            }

            // Update SrcInfoCache with new data
            state
                .srcinfo_cache
                .insert(pkg.pkgbase.clone(), srcinfo, new_commit.clone());

            // Auto-approve if configured
            if pkg.auto_approve {
                state
                    .db
                    .approve_package(&pkg.pkgbase, &new_commit, true)
                    .await?;
            }

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
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "updates": updates,
                "count": updates.len(),
            }))?
        );
    } else if updates.is_empty() {
        println!("\nAll packages up to date");
    } else {
        let names: Vec<&str> = updates
            .iter()
            .filter_map(|u| u.get("pkgbase").and_then(|v| v.as_str()))
            .collect();
        println!(
            "\n{} package(s) with updates: {}",
            updates.len(),
            names.join(", ")
        );
    }

    Ok(())
}

async fn cmd_rebuild_image(state: &CliState) -> Result<()> {
    println!("Rebuilding base container image...");
    state.build_system.build_base_image()?;
    println!("Done");

    Ok(())
}

async fn cmd_status(state: &CliState, json_output: bool) -> Result<()> {
    // Gather status info
    let podman_available = BuildSystem::check_podman().is_ok();
    let image_exists = state.build_system.image_exists().unwrap_or(false);
    let repo_stats = state.repo.stats().ok();

    // Package counts by derived status
    let packages = state.db.list_packages().await?;
    let active_builds: HashSet<String> = HashSet::new();
    let aur_clones = state.config.resolved_cache().aur_clones.clone();

    let mut by_status: HashMap<String, usize> = HashMap::new();
    for pkg in &packages {
        let enriched = enrich_package(
            pkg,
            &state.db,
            &state.srcinfo_cache,
            &state.repo_state,
            &active_builds,
            &aur_clones,
        )
        .await;
        *by_status.entry(enriched.status.to_string()).or_default() += 1;
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
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
            }))?
        );
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
            println!(
                "Repository: {} ({} packages, {} bytes)",
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

async fn cmd_mark(state: &CliState, name: &str, explicit: bool, asdeps: bool) -> Result<()> {
    let pkg = state
        .resolve_package(name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Package not found"))?;

    let new_reason = if explicit {
        InstallReason::Explicit
    } else if asdeps {
        InstallReason::Dependency
    } else {
        anyhow::bail!("Must specify either --explicit or --asdeps");
    };

    if pkg.install_reason == new_reason {
        println!("{} is already marked as {}", pkg.pkgbase, new_reason);
        return Ok(());
    }

    state
        .db
        .update_install_reason(&pkg.pkgbase, new_reason)
        .await?;
    println!(
        "Changed {} install reason: {} -> {}",
        pkg.pkgbase, pkg.install_reason, new_reason
    );

    Ok(())
}

/// Package being imported (intermediate state for cmd_import)
struct ImportedPackage {
    pkg: models::Package,
    pkg_entries: Vec<repo::db_parser::RepoPkgEntry>,
    has_upstream: bool,
    in_official_repos: bool,
    srcinfo_opt: Option<models::SrcInfo>,
}

fn cmd_config(
    action: ConfigAction,
    config: &Config,
    custom_path: Option<&std::path::Path>,
) -> Result<()> {
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

async fn cmd_import(
    state: &mut CliState,
    db_path: &std::path::Path,
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    use anyhow::Context;
    use repo::db_parser::parse_repo_db;

    // Parse the repository database
    if !json_output {
        println!("Parsing repository database: {}", db_path.display());
    }

    let repo_contents = parse_repo_db(db_path)
        .with_context(|| format!("Failed to parse repo database: {}", db_path.display()))?;

    if !json_output {
        println!(
            "Found {} packages in {} pkgbases",
            repo_contents.packages.len(),
            repo_contents.by_pkgbase.len()
        );
    }

    // Get tracked packages
    let packages = state.db.list_packages().await?;
    let tracked_pkgbases: HashSet<String> = packages.iter().map(|p| p.pkgbase.clone()).collect();

    // Process each pkgbase
    let mut imported: Vec<ImportedPackage> = Vec::new();
    let mut skipped_tracked = 0;
    let mut no_upstream = 0;
    let mut _clone_errors: Vec<(String, String)> = Vec::new();

    let total = repo_contents.by_pkgbase.len();
    let resolved_cache = state.config.resolved_cache();

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
        let repo_version = entries
            .first()
            .map(|e| e.version.clone())
            .unwrap_or_default();

        // Try to clone from AUR
        let clone_path = resolved_cache.aur_clones.join(pkgbase);

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
                        _clone_errors.push((pkgbase.to_string(), e.to_string()));
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
            match state.aur.info_single(pkgbase).await {
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

        // Determine if there's an update (for display)
        let _has_update = if has_upstream {
            if let Some(ref srcinfo) = srcinfo_opt {
                let aur_version = srcinfo.full_version();
                if aur_version != repo_version {
                    if !json_output {
                        print!("(update: {} -> {}) ", repo_version, aur_version);
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if !json_output {
            println!();
        }

        // Create package record (V2 -- no status, pkgver, current_commit, etc.)
        let now = chrono::Utc::now();

        let pkg = models::Package {
            pkgbase: pkgbase.to_string(),
            // Auto-approve imported packages (they were already built)
            approved_commit: current_commit.clone(),
            install_reason: InstallReason::Explicit, // Will be updated later
            auto_approve: false,
            added_at: now,
            updated_at: now,
        };

        // Add to SrcInfoCache if we have srcinfo
        if let (Some(srcinfo), Some(commit)) = (&srcinfo_opt, &current_commit) {
            state
                .srcinfo_cache
                .insert(pkgbase.to_string(), srcinfo.clone(), commit.clone());
        }

        imported.push(ImportedPackage {
            pkg,
            pkg_entries: entries.clone(),
            has_upstream,
            in_official_repos: false,
            srcinfo_opt,
        });
    }

    // Infer install reasons from dependency graph
    infer_install_reasons_v2(&mut imported);

    // Count stats
    let explicit_count = imported
        .iter()
        .filter(|p| p.pkg.install_reason == InstallReason::Explicit)
        .count();
    let dep_count = imported
        .iter()
        .filter(|p| p.pkg.install_reason == InstallReason::Dependency)
        .count();
    // For update count, derive status for each imported package
    let active_builds: HashSet<String> = HashSet::new();
    let aur_clones = state.config.resolved_cache().aur_clones.clone();
    let mut update_count = 0;
    for imp in &imported {
        let enriched = enrich_package(
            &imp.pkg,
            &state.db,
            &state.srcinfo_cache,
            &state.repo_state,
            &active_builds,
            &aur_clones,
        )
        .await;
        if enriched.status == DerivedStatus::UpdateAvailable {
            update_count += 1;
        }
    }
    let deprecated_count = imported
        .iter()
        .filter(|p| !p.has_upstream && p.in_official_repos)
        .count();
    let local_only_count = imported
        .iter()
        .filter(|p| !p.has_upstream && !p.in_official_repos)
        .count();

    if dry_run {
        // Print what would be imported
        if json_output {
            let pkgs: Vec<serde_json::Value> = imported
                .iter()
                .map(|imp| {
                    let pkgver = imp
                        .srcinfo_opt
                        .as_ref()
                        .map(|s| s.full_version())
                        .or_else(|| imp.pkg_entries.first().map(|e| e.version.clone()));
                    let pkgnames: Vec<&str> = imp
                        .srcinfo_opt
                        .as_ref()
                        .map(|s| s.packages.iter().map(|p| p.pkgname.as_str()).collect())
                        .unwrap_or_else(|| {
                            imp.pkg_entries.iter().map(|e| e.name.as_str()).collect()
                        });
                    serde_json::json!({
                        "pkgbase": imp.pkg.pkgbase,
                        "version": pkgver,
                        "install_reason": imp.pkg.install_reason.to_string(),
                        "has_upstream": imp.has_upstream,
                        "in_official_repos": imp.in_official_repos,
                        "pkgnames": pkgnames,
                    })
                })
                .collect();

            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "would_import": pkgs.len(),
                    "skipped_already_tracked": skipped_tracked,
                    "explicit": explicit_count,
                    "dependencies": dep_count,
                    "updates_available": update_count,
                    "no_upstream": no_upstream,
                    "deprecated": deprecated_count,
                    "local_only": local_only_count,
                    "packages": pkgs,
                }))?
            );
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
                println!(
                    "    - Local only (not in AUR or official): {}",
                    local_only_count
                );
            }
            println!("Skipped (already tracked): {}", skipped_tracked);

            if !imported.is_empty() {
                println!("\nPackages to import:");
                for imp in &imported {
                    let pkgver = imp
                        .srcinfo_opt
                        .as_ref()
                        .map(|s| s.full_version())
                        .or_else(|| imp.pkg_entries.first().map(|e| e.version.clone()));
                    let reason = match imp.pkg.install_reason {
                        InstallReason::Explicit => "explicit",
                        InstallReason::Dependency => "dependency",
                    };
                    let status_note = if !imp.has_upstream && imp.in_official_repos {
                        " (DEPRECATED - now in official repos)"
                    } else if !imp.has_upstream {
                        " (local only)"
                    } else {
                        ""
                    };
                    println!(
                        "  {} {} [{}]{}",
                        imp.pkg.pkgbase,
                        pkgver.as_deref().unwrap_or("-"),
                        reason,
                        status_note
                    );
                }
            }
        }

        return Ok(());
    }

    // Actually insert packages
    if !json_output {
        println!("\nInserting packages into database...");
    }

    for imp in &imported {
        state.db.insert_package(&imp.pkg).await?;
    }

    // Print summary
    if json_output {
        let pkgs: Vec<serde_json::Value> = imported
            .iter()
            .map(|imp| {
                let pkgver = imp
                    .srcinfo_opt
                    .as_ref()
                    .map(|s| s.full_version())
                    .or_else(|| imp.pkg_entries.first().map(|e| e.version.clone()));
                serde_json::json!({
                    "pkgbase": imp.pkg.pkgbase,
                    "version": pkgver,
                    "install_reason": imp.pkg.install_reason.to_string(),
                    "has_upstream": imp.has_upstream,
                    "in_official_repos": imp.in_official_repos,
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": "ok",
                "imported": pkgs.len(),
                "skipped_already_tracked": skipped_tracked,
                "explicit": explicit_count,
                "dependencies": dep_count,
                "updates_available": update_count,
                "no_upstream": no_upstream,
                "deprecated": deprecated_count,
                "local_only": local_only_count,
                "packages": pkgs,
            }))?
        );
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
        let deprecated_pkgs: Vec<_> = imported
            .iter()
            .filter(|p| !p.has_upstream && p.in_official_repos)
            .collect();
        if !deprecated_pkgs.is_empty() {
            println!("\nDeprecated packages (consider removing - now in official repos):");
            for imp in deprecated_pkgs {
                let pkgver = imp
                    .srcinfo_opt
                    .as_ref()
                    .map(|s| s.full_version())
                    .or_else(|| imp.pkg_entries.first().map(|e| e.version.clone()));
                println!("  {} {}", imp.pkg.pkgbase, pkgver.as_deref().unwrap_or("-"));
            }
        }

        // Show local-only packages
        let local_only_pkgs: Vec<_> = imported
            .iter()
            .filter(|p| !p.has_upstream && !p.in_official_repos)
            .collect();
        if !local_only_pkgs.is_empty() && local_only_pkgs.len() <= 15 {
            println!("\nLocal-only packages (no AUR or official repo upstream):");
            for imp in local_only_pkgs {
                let pkgver = imp
                    .srcinfo_opt
                    .as_ref()
                    .map(|s| s.full_version())
                    .or_else(|| imp.pkg_entries.first().map(|e| e.version.clone()));
                println!("  {} {}", imp.pkg.pkgbase, pkgver.as_deref().unwrap_or("-"));
            }
        }
    }

    // Check if config should be updated
    if !json_output && !imported.is_empty() {
        let repo_dir = db_path.parent().unwrap_or(std::path::Path::new("."));

        // Extract repo name from database filename
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
        let config_differs =
            state.config.repo.path != repo_dir || state.config.repo.name != repo_name;

        if config_differs {
            println!();
            println!("Your current config points to a different repository:");
            println!(
                "  Current: {} ({})",
                state.config.repo.path.display(),
                state.config.repo.name
            );
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
fn check_official_repos(pkgnames: &[String]) -> HashSet<String> {
    let mut in_official: HashSet<String> = HashSet::new();

    if pkgnames.is_empty() {
        return in_official;
    }

    for pkgname in pkgnames {
        let output = std::process::Command::new("pacman")
            .args(["-Si", pkgname])
            .output();

        if let Ok(output) = output
            && output.status.success()
        {
            in_official.insert(pkgname.clone());
        }
    }

    in_official
}

/// Infer install reasons and check official repos for imported packages
///
/// Uses multiple heuristics:
/// 1. Packages depended upon by others in the set -> Dependency
/// 2. Compat packages (-compat-libs, -shim, version suffixes) without upstream -> Dependency
/// 3. Packages without upstream that exist in official repos -> mark in_official_repos
/// 4. Everything else -> Explicit (conservative default)
fn infer_install_reasons_v2(packages: &mut [ImportedPackage]) {
    use repo::db_parser::strip_version_constraint;

    // Collect all pkgnames for packages without upstream to check official repos
    let pkgnames_to_check: Vec<String> = packages
        .iter()
        .filter(|imp| !imp.has_upstream)
        .flat_map(|imp| imp.pkg_entries.iter().map(|e| e.name.clone()))
        .collect();

    // Check which packages are in official repos
    let in_official = check_official_repos(&pkgnames_to_check);

    // Mark packages as in_official_repos if any of their pkgnames are in official repos
    for imp in packages.iter_mut() {
        if !imp.has_upstream {
            let is_in_official = imp
                .pkg_entries
                .iter()
                .any(|e| in_official.contains(&e.name));
            imp.in_official_repos = is_in_official;
        }
    }

    // Build pkgname -> pkgbase map (including provides)
    let mut pkgname_to_pkgbase: HashMap<String, String> = HashMap::new();
    for imp in packages.iter() {
        for entry in &imp.pkg_entries {
            pkgname_to_pkgbase.insert(entry.name.clone(), imp.pkg.pkgbase.clone());
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
                if let Some(dep_pkgbase) = pkgname_to_pkgbase.get(dep_name)
                    && *dep_pkgbase != imp.pkg.pkgbase
                {
                    reverse_deps
                        .entry(dep_pkgbase.clone())
                        .or_default()
                        .insert(imp.pkg.pkgbase.clone());
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

        if has_dependents {
            imp.pkg.install_reason = InstallReason::Dependency;
            continue;
        }

        let pkgbase = &imp.pkg.pkgbase;
        let looks_like_compat = pkgbase.ends_with("-compat-libs")
            || pkgbase.ends_with("-shim")
            || pkgbase
                .chars()
                .last()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
                && pkgbase.chars().filter(|c| c.is_ascii_digit()).count() >= 2;

        if looks_like_compat && !imp.has_upstream {
            imp.pkg.install_reason = InstallReason::Dependency;
        } else {
            imp.pkg.install_reason = InstallReason::Explicit;
        }
    }
}
