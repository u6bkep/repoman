use anyhow::{Context, Result};
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::path::Path;

use crate::models::{Build, BuildStatus, InstallReason, Package, PackageName, PackageStatus};

/// Database connection pool
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Connect to database, creating it if necessary
    pub async fn connect(path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create database directory: {}", parent.display()))?;
        }

        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .with_context(|| format!("Failed to connect to database: {}", path.display()))?;

        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    /// Run database migrations
    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS packages (
                pkgbase TEXT PRIMARY KEY,
                aur_url TEXT NOT NULL,
                local_path TEXT NOT NULL,
                approved_commit TEXT,
                current_commit TEXT,
                pkgver TEXT,
                status TEXT NOT NULL DEFAULT 'pending_review',
                install_reason TEXT NOT NULL DEFAULT 'explicit',
                auto_approve BOOLEAN NOT NULL DEFAULT FALSE,
                rejection_reason TEXT,
                added_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Migration: add install_reason column to existing databases
        // SQLite doesn't support ADD COLUMN IF NOT EXISTS, so we check first
        let has_column: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'install_reason'"
        )
        .fetch_optional(&self.pool)
        .await?;

        if has_column.is_none() {
            sqlx::query(
                "ALTER TABLE packages ADD COLUMN install_reason TEXT NOT NULL DEFAULT 'explicit'"
            )
            .execute(&self.pool)
            .await?;
        }

        // Migration: add rejection_reason column
        let has_rejection_reason: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'rejection_reason'"
        )
        .fetch_optional(&self.pool)
        .await?;

        if has_rejection_reason.is_none() {
            sqlx::query("ALTER TABLE packages ADD COLUMN rejection_reason TEXT")
                .execute(&self.pool)
                .await?;
        }

        // Migration: add has_upstream column
        let has_upstream: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'has_upstream'"
        )
        .fetch_optional(&self.pool)
        .await?;

        if has_upstream.is_none() {
            sqlx::query("ALTER TABLE packages ADD COLUMN has_upstream BOOLEAN NOT NULL DEFAULT TRUE")
                .execute(&self.pool)
                .await?;
        }

        // Migration: add in_official_repos column
        let in_official_repos: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'in_official_repos'"
        )
        .fetch_optional(&self.pool)
        .await?;

        if in_official_repos.is_none() {
            sqlx::query("ALTER TABLE packages ADD COLUMN in_official_repos BOOLEAN NOT NULL DEFAULT FALSE")
                .execute(&self.pool)
                .await?;
        }

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS package_names (
                pkgname TEXT PRIMARY KEY,
                pkgbase TEXT NOT NULL REFERENCES packages(pkgbase) ON DELETE CASCADE,
                pkgdesc TEXT,
                pkgver TEXT,
                depends TEXT,
                optdepends TEXT,
                provides TEXT,
                conflicts TEXT,
                repo_version TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS builds (
                id TEXT PRIMARY KEY,
                pkgbase TEXT NOT NULL REFERENCES packages(pkgbase),
                git_commit TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                finished_at TEXT,
                log_path TEXT,
                outputs TEXT,
                error_message TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS approvals (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pkgbase TEXT NOT NULL REFERENCES packages(pkgbase),
                git_commit TEXT NOT NULL,
                approved_at TEXT NOT NULL,
                approved_by TEXT,
                auto_approved BOOLEAN NOT NULL DEFAULT FALSE
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_package_names_pkgbase
            ON package_names(pkgbase)
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_builds_pkgbase
            ON builds(pkgbase)
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // ==================== Package operations ====================

    /// Insert a new package
    pub async fn insert_package(&self, pkg: &Package) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO packages (pkgbase, aur_url, local_path, approved_commit, current_commit, pkgver, status, install_reason, auto_approve, has_upstream, in_official_repos, added_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&pkg.pkgbase)
        .bind(&pkg.aur_url)
        .bind(&pkg.local_path)
        .bind(&pkg.approved_commit)
        .bind(&pkg.current_commit)
        .bind(&pkg.pkgver)
        .bind(pkg.status.to_string())
        .bind(pkg.install_reason.to_string())
        .bind(pkg.auto_approve)
        .bind(pkg.has_upstream)
        .bind(pkg.in_official_repos)
        .bind(pkg.added_at.to_rfc3339())
        .bind(pkg.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a package by pkgbase
    pub async fn get_package(&self, pkgbase: &str) -> Result<Option<Package>> {
        let row = sqlx::query_as::<_, PackageRow>(
            "SELECT * FROM packages WHERE pkgbase = ?",
        )
        .bind(pkgbase)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.into()))
    }

    /// Get a package by any of its pkgnames
    pub async fn get_package_by_pkgname(&self, pkgname: &str) -> Result<Option<Package>> {
        let row = sqlx::query_as::<_, PackageRow>(
            r#"
            SELECT p.* FROM packages p
            JOIN package_names pn ON p.pkgbase = pn.pkgbase
            WHERE pn.pkgname = ?
            "#,
        )
        .bind(pkgname)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.into()))
    }

    /// List all packages
    pub async fn list_packages(&self) -> Result<Vec<Package>> {
        let rows = sqlx::query_as::<_, PackageRow>("SELECT * FROM packages ORDER BY pkgbase")
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// List packages by status
    pub async fn list_packages_by_status(&self, status: PackageStatus) -> Result<Vec<Package>> {
        let rows = sqlx::query_as::<_, PackageRow>(
            "SELECT * FROM packages WHERE status = ? ORDER BY pkgbase",
        )
        .bind(status.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// List packages by install reason
    pub async fn list_packages_by_reason(&self, reason: InstallReason) -> Result<Vec<Package>> {
        let rows = sqlx::query_as::<_, PackageRow>(
            "SELECT * FROM packages WHERE install_reason = ? ORDER BY pkgbase",
        )
        .bind(reason.to_string())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// Update package install reason
    pub async fn update_install_reason(&self, pkgbase: &str, reason: InstallReason) -> Result<()> {
        sqlx::query("UPDATE packages SET install_reason = ?, updated_at = ? WHERE pkgbase = ?")
            .bind(reason.to_string())
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(pkgbase)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update package auto_approve setting
    pub async fn update_auto_approve(&self, pkgbase: &str, auto_approve: bool) -> Result<()> {
        sqlx::query("UPDATE packages SET auto_approve = ?, updated_at = ? WHERE pkgbase = ?")
            .bind(auto_approve)
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(pkgbase)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// List orphaned packages (dependencies that are no longer needed)
    /// A package is orphaned if:
    /// - It has install_reason = 'dependency'
    /// - No explicitly installed package depends on it (directly or transitively)
    pub async fn list_orphans(&self) -> Result<Vec<Package>> {
        // Get all packages
        let all_packages = self.list_packages().await?;

        // Get all package names with their dependencies
        let mut all_deps: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Collect dependencies of all explicit packages
        for pkg in &all_packages {
            if pkg.install_reason == InstallReason::Explicit {
                let names = self.get_package_names(&pkg.pkgbase).await?;
                for name in names {
                    if let Some(deps_json) = &name.depends {
                        if let Ok(deps) = serde_json::from_str::<Vec<String>>(deps_json) {
                            for dep in deps {
                                // Extract package name from versioned dependency (e.g., "foo>=1.0" -> "foo")
                                let dep_name = dep.split(|c| c == '>' || c == '<' || c == '=').next().unwrap_or(&dep);
                                all_deps.insert(dep_name.to_string());
                            }
                        }
                    }
                }
            }
        }

        // Find dependency packages that aren't depended on by any explicit package
        let mut orphans = Vec::new();
        for pkg in all_packages {
            if pkg.install_reason == InstallReason::Dependency {
                let names = self.get_package_names(&pkg.pkgbase).await?;
                let is_needed = names.iter().any(|n| all_deps.contains(&n.pkgname));
                if !is_needed {
                    orphans.push(pkg);
                }
            }
        }

        Ok(orphans)
    }

    /// Update package status
    pub async fn update_package_status(&self, pkgbase: &str, status: PackageStatus) -> Result<()> {
        sqlx::query("UPDATE packages SET status = ?, updated_at = ? WHERE pkgbase = ?")
            .bind(status.to_string())
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(pkgbase)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Reject a package with an optional reason
    pub async fn reject_package(&self, pkgbase: &str, reason: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE packages SET status = ?, rejection_reason = ?, updated_at = ? WHERE pkgbase = ?",
        )
        .bind(PackageStatus::Rejected.to_string())
        .bind(reason)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(pkgbase)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update package commit info
    pub async fn update_package_commit(
        &self,
        pkgbase: &str,
        current_commit: &str,
        pkgver: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE packages SET current_commit = ?, pkgver = ?, updated_at = ? WHERE pkgbase = ?",
        )
        .bind(current_commit)
        .bind(pkgver)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(pkgbase)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Approve a package (set approved_commit to current_commit)
    pub async fn approve_package(&self, pkgbase: &str, auto: bool, specific_commit: Option<&str>) -> Result<()> {
        let pkg = self.get_package(pkgbase).await?
            .ok_or_else(|| anyhow::anyhow!("Package not found: {}", pkgbase))?;

        // Use specific commit if provided, otherwise use current commit
        let commit = match specific_commit {
            Some(c) => c.to_string(),
            None => pkg.current_commit
                .ok_or_else(|| anyhow::anyhow!("Package has no current commit"))?,
        };

        // Update package
        sqlx::query(
            "UPDATE packages SET approved_commit = ?, status = ?, updated_at = ? WHERE pkgbase = ?",
        )
        .bind(&commit)
        .bind(PackageStatus::Approved.to_string())
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(pkgbase)
        .execute(&self.pool)
        .await?;

        // Record approval
        sqlx::query(
            "INSERT INTO approvals (pkgbase, git_commit, approved_at, auto_approved) VALUES (?, ?, ?, ?)",
        )
        .bind(pkgbase)
        .bind(&commit)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(auto)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete a package
    pub async fn delete_package(&self, pkgbase: &str) -> Result<()> {
        sqlx::query("DELETE FROM packages WHERE pkgbase = ?")
            .bind(pkgbase)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ==================== Package names operations ====================

    /// Insert package names for a pkgbase
    pub async fn insert_package_names(&self, names: &[PackageName]) -> Result<()> {
        for name in names {
            sqlx::query(
                r#"
                INSERT OR REPLACE INTO package_names
                (pkgname, pkgbase, pkgdesc, pkgver, depends, optdepends, provides, conflicts, repo_version)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(&name.pkgname)
            .bind(&name.pkgbase)
            .bind(&name.pkgdesc)
            .bind(&name.pkgver)
            .bind(&name.depends)
            .bind(&name.optdepends)
            .bind(&name.provides)
            .bind(&name.conflicts)
            .bind(&name.repo_version)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Get all package names for a pkgbase
    pub async fn get_package_names(&self, pkgbase: &str) -> Result<Vec<PackageName>> {
        let rows = sqlx::query_as::<_, PackageNameRow>(
            "SELECT * FROM package_names WHERE pkgbase = ? ORDER BY pkgname",
        )
        .bind(pkgbase)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// Resolve pkgname to pkgbase
    pub async fn resolve_pkgbase(&self, pkgname: &str) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT pkgbase FROM package_names WHERE pkgname = ?",
        )
        .bind(pkgname)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(pkgbase,)| pkgbase))
    }

    // ==================== Build operations ====================

    /// Insert a new build
    pub async fn insert_build(&self, build: &Build) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO builds (id, pkgbase, git_commit, status, started_at, finished_at, log_path, outputs, error_message)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&build.id)
        .bind(&build.pkgbase)
        .bind(&build.commit)
        .bind(build.status.to_string())
        .bind(build.started_at.to_rfc3339())
        .bind(build.finished_at.map(|t| t.to_rfc3339()))
        .bind(&build.log_path)
        .bind(&build.outputs)
        .bind(&build.error_message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a build by ID
    pub async fn get_build(&self, id: &str) -> Result<Option<Build>> {
        let row = sqlx::query_as::<_, BuildRow>("SELECT * FROM builds WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| r.into()))
    }

    /// List builds for a package with pagination
    pub async fn list_builds(&self, pkgbase: &str, limit: i32, offset: i32) -> Result<Vec<Build>> {
        let rows = sqlx::query_as::<_, BuildRow>(
            "SELECT * FROM builds WHERE pkgbase = ? ORDER BY started_at DESC LIMIT ? OFFSET ?",
        )
        .bind(pkgbase)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// Update build status
    pub async fn update_build_status(
        &self,
        id: &str,
        status: BuildStatus,
        error_message: Option<&str>,
    ) -> Result<()> {
        let finished_at = match status {
            BuildStatus::Success | BuildStatus::Failed | BuildStatus::Cancelled => {
                Some(chrono::Utc::now().to_rfc3339())
            }
            _ => None,
        };

        sqlx::query(
            "UPDATE builds SET status = ?, finished_at = ?, error_message = ? WHERE id = ?",
        )
        .bind(status.to_string())
        .bind(finished_at)
        .bind(error_message)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update build outputs
    pub async fn update_build_outputs(&self, id: &str, outputs: &str) -> Result<()> {
        sqlx::query("UPDATE builds SET outputs = ? WHERE id = ?")
            .bind(outputs)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

// ==================== Row types for sqlx ====================

#[derive(sqlx::FromRow)]
struct PackageRow {
    pkgbase: String,
    aur_url: String,
    local_path: String,
    approved_commit: Option<String>,
    current_commit: Option<String>,
    pkgver: Option<String>,
    status: String,
    install_reason: String,
    auto_approve: bool,
    has_upstream: bool,
    in_official_repos: bool,
    rejection_reason: Option<String>,
    added_at: String,
    updated_at: String,
}

impl From<PackageRow> for Package {
    fn from(row: PackageRow) -> Self {
        Self {
            pkgbase: row.pkgbase,
            aur_url: row.aur_url,
            local_path: row.local_path,
            approved_commit: row.approved_commit,
            current_commit: row.current_commit,
            pkgver: row.pkgver,
            status: parse_status(&row.status),
            install_reason: parse_install_reason(&row.install_reason),
            auto_approve: row.auto_approve,
            has_upstream: row.has_upstream,
            in_official_repos: row.in_official_repos,
            rejection_reason: row.rejection_reason,
            added_at: chrono::DateTime::parse_from_rfc3339(&row.added_at)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            updated_at: chrono::DateTime::parse_from_rfc3339(&row.updated_at)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
        }
    }
}

fn parse_status(s: &str) -> PackageStatus {
    match s {
        "pending_review" => PackageStatus::PendingReview,
        "approved" => PackageStatus::Approved,
        "building" => PackageStatus::Building,
        "failed" => PackageStatus::Failed,
        "up_to_date" => PackageStatus::UpToDate,
        "update_available" => PackageStatus::UpdateAvailable,
        "rejected" => PackageStatus::Rejected,
        _ => PackageStatus::PendingReview,
    }
}

fn parse_install_reason(s: &str) -> InstallReason {
    match s {
        "dependency" => InstallReason::Dependency,
        _ => InstallReason::Explicit,
    }
}

#[derive(sqlx::FromRow)]
struct PackageNameRow {
    pkgname: String,
    pkgbase: String,
    pkgdesc: Option<String>,
    pkgver: Option<String>,
    depends: Option<String>,
    optdepends: Option<String>,
    provides: Option<String>,
    conflicts: Option<String>,
    repo_version: Option<String>,
}

impl From<PackageNameRow> for PackageName {
    fn from(row: PackageNameRow) -> Self {
        Self {
            pkgname: row.pkgname,
            pkgbase: row.pkgbase,
            pkgdesc: row.pkgdesc,
            pkgver: row.pkgver,
            depends: row.depends,
            optdepends: row.optdepends,
            provides: row.provides,
            conflicts: row.conflicts,
            repo_version: row.repo_version,
        }
    }
}

#[derive(sqlx::FromRow)]
struct BuildRow {
    id: String,
    pkgbase: String,
    git_commit: String,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    log_path: Option<String>,
    outputs: Option<String>,
    error_message: Option<String>,
}

impl From<BuildRow> for Build {
    fn from(row: BuildRow) -> Self {
        Self {
            id: row.id,
            pkgbase: row.pkgbase,
            commit: row.git_commit,
            status: parse_build_status(&row.status),
            started_at: chrono::DateTime::parse_from_rfc3339(&row.started_at)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            finished_at: row.finished_at.and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(&s)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .ok()
            }),
            log_path: row.log_path,
            outputs: row.outputs,
            error_message: row.error_message,
        }
    }
}

fn parse_build_status(s: &str) -> BuildStatus {
    match s {
        "pending" => BuildStatus::Pending,
        "running" => BuildStatus::Running,
        "success" => BuildStatus::Success,
        "failed" => BuildStatus::Failed,
        "cancelled" => BuildStatus::Cancelled,
        _ => BuildStatus::Pending,
    }
}
