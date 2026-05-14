use anyhow::{Context, Result};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use std::path::Path;

use crate::models::{Build, BuildStatus, InstallReason, Package};

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
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create database directory: {}", parent.display())
            })?;
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
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'install_reason'",
        )
        .fetch_optional(&self.pool)
        .await?;

        if has_column.is_none() {
            sqlx::query(
                "ALTER TABLE packages ADD COLUMN install_reason TEXT NOT NULL DEFAULT 'explicit'",
            )
            .execute(&self.pool)
            .await?;
        }

        // Migration: add rejection_reason column
        let has_rejection_reason: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'rejection_reason'",
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
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'has_upstream'",
        )
        .fetch_optional(&self.pool)
        .await?;

        if has_upstream.is_none() {
            sqlx::query(
                "ALTER TABLE packages ADD COLUMN has_upstream BOOLEAN NOT NULL DEFAULT TRUE",
            )
            .execute(&self.pool)
            .await?;
        }

        // Migration: add in_official_repos column
        let in_official_repos: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM pragma_table_info('packages') WHERE name = 'in_official_repos'",
        )
        .fetch_optional(&self.pool)
        .await?;

        if in_official_repos.is_none() {
            sqlx::query(
                "ALTER TABLE packages ADD COLUMN in_official_repos BOOLEAN NOT NULL DEFAULT FALSE",
            )
            .execute(&self.pool)
            .await?;
        }

        // Migration: drop local_path column (path is now derived from config + pkgbase)
        let has_local_path: Option<(i32,)> =
            sqlx::query_as("SELECT 1 FROM pragma_table_info('packages') WHERE name = 'local_path'")
                .fetch_optional(&self.pool)
                .await?;

        if has_local_path.is_some() {
            sqlx::query("ALTER TABLE packages DROP COLUMN local_path")
                .execute(&self.pool)
                .await?;
        }

        // Migration: drop aur_url column (URL is now derived from pkgbase)
        let has_aur_url: Option<(i32,)> =
            sqlx::query_as("SELECT 1 FROM pragma_table_info('packages') WHERE name = 'aur_url'")
                .fetch_optional(&self.pool)
                .await?;

        if has_aur_url.is_some() {
            sqlx::query("ALTER TABLE packages DROP COLUMN aur_url")
                .execute(&self.pool)
                .await?;
        }

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
            CREATE INDEX IF NOT EXISTS idx_builds_pkgbase
            ON builds(pkgbase)
            "#,
        )
        .execute(&self.pool)
        .await?;

        // ==================== V2 Migration ====================
        // Drop package_names table and index (data now comes from .SRCINFO cache)
        sqlx::query("DROP INDEX IF EXISTS idx_package_names_pkgbase")
            .execute(&self.pool)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS package_names")
            .execute(&self.pool)
            .await?;

        // Drop columns removed in V2 (status, current_commit, pkgver, has_upstream, in_official_repos, rejection_reason)
        let v2_drop_columns = [
            "current_commit",
            "pkgver",
            "status",
            "has_upstream",
            "in_official_repos",
            "rejection_reason",
        ];
        for col in &v2_drop_columns {
            let has_col: Option<(i32,)> = sqlx::query_as(&format!(
                "SELECT 1 FROM pragma_table_info('packages') WHERE name = '{}'",
                col
            ))
            .fetch_optional(&self.pool)
            .await?;

            if has_col.is_some() {
                sqlx::query(&format!("ALTER TABLE packages DROP COLUMN {}", col))
                    .execute(&self.pool)
                    .await?;
            }
        }

        Ok(())
    }

    // ==================== Package operations ====================

    /// Insert a package into the database
    pub async fn insert_package(&self, pkg: &Package) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO packages (pkgbase, install_reason, auto_approve, approved_commit, added_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&pkg.pkgbase)
        .bind(pkg.install_reason.to_string())
        .bind(pkg.auto_approve)
        .bind(&pkg.approved_commit)
        .bind(pkg.added_at.to_rfc3339())
        .bind(pkg.updated_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a package by pkgbase
    pub async fn get_package(&self, pkgbase: &str) -> Result<Option<Package>> {
        let row = sqlx::query_as::<_, PackageRow>("SELECT * FROM packages WHERE pkgbase = ?")
            .bind(pkgbase)
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

    /// Approve a package by setting approved_commit and recording the approval
    pub async fn approve_package(&self, pkgbase: &str, commit: &str, auto: bool) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();

        let mut tx = self.pool.begin().await?;

        // Update approved_commit and updated_at
        sqlx::query("UPDATE packages SET approved_commit = ?, updated_at = ? WHERE pkgbase = ?")
            .bind(commit)
            .bind(&now)
            .bind(pkgbase)
            .execute(&mut *tx)
            .await?;

        // Record approval
        sqlx::query(
            "INSERT INTO approvals (pkgbase, git_commit, approved_at, auto_approved) VALUES (?, ?, ?, ?)",
        )
        .bind(pkgbase)
        .bind(commit)
        .bind(&now)
        .bind(auto)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
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

    /// List all builds across all packages with optional status filter
    pub async fn list_all_builds(
        &self,
        limit: i32,
        offset: i32,
        status: Option<&str>,
    ) -> Result<Vec<Build>> {
        let rows = if let Some(status) = status {
            sqlx::query_as::<_, BuildRow>(
                "SELECT * FROM builds WHERE status = ? ORDER BY started_at DESC LIMIT ? OFFSET ?",
            )
            .bind(status)
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, BuildRow>(
                "SELECT * FROM builds ORDER BY started_at DESC LIMIT ? OFFSET ?",
            )
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?
        };

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

    /// Recover stale builds left in pending/running state (e.g. after server crash).
    /// Marks them as cancelled. Returns the number of builds recovered.
    /// Note: In V2, package status is derived at runtime, so no package status reset is needed.
    pub async fn recover_stale_builds(&self) -> Result<u32> {
        let now = chrono::Utc::now().to_rfc3339();

        // Cancel stale builds
        let build_result = sqlx::query(
            "UPDATE builds SET status = 'cancelled', error_message = 'Server restarted during build', finished_at = ? WHERE status IN ('pending', 'running')"
        )
        .bind(&now)
        .execute(&self.pool)
        .await?;

        let recovered = build_result.rows_affected() as u32;

        Ok(recovered)
    }

    /// List all build log file names (base names only) from the builds table
    pub async fn list_build_log_filenames(&self) -> Result<std::collections::HashSet<String>> {
        let rows: Vec<(Option<String>,)> =
            sqlx::query_as("SELECT log_path FROM builds WHERE log_path IS NOT NULL")
                .fetch_all(&self.pool)
                .await?;

        let names = rows
            .into_iter()
            .filter_map(|(path,)| {
                path.and_then(|p| {
                    std::path::Path::new(&p)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
            })
            .collect();

        Ok(names)
    }

    /// Get the latest build for a specific package and commit hash.
    /// Used for status derivation: determines whether the approved commit has been built.
    pub async fn get_latest_build_for_commit(
        &self,
        pkgbase: &str,
        commit: &str,
    ) -> Result<Option<Build>> {
        let row = sqlx::query_as::<_, BuildRow>(
            "SELECT * FROM builds WHERE pkgbase = ? AND git_commit = ? ORDER BY started_at DESC LIMIT 1",
        )
        .bind(pkgbase)
        .bind(commit)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|r| r.into()))
    }
}

// ==================== Row types for sqlx ====================

#[derive(sqlx::FromRow)]
struct PackageRow {
    pkgbase: String,
    install_reason: String,
    auto_approve: bool,
    approved_commit: Option<String>,
    added_at: String,
    updated_at: String,
}

impl From<PackageRow> for Package {
    fn from(row: PackageRow) -> Self {
        Self {
            pkgbase: row.pkgbase,
            install_reason: parse_install_reason(&row.install_reason),
            auto_approve: row.auto_approve,
            approved_commit: row.approved_commit,
            added_at: chrono::DateTime::parse_from_rfc3339(&row.added_at)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
            updated_at: chrono::DateTime::parse_from_rfc3339(&row.updated_at)
                .map(|t| t.with_timezone(&chrono::Utc))
                .unwrap_or_else(|_| chrono::Utc::now()),
        }
    }
}

fn parse_install_reason(s: &str) -> InstallReason {
    match s {
        "dependency" => InstallReason::Dependency,
        _ => InstallReason::Explicit,
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
