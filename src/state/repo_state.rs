use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::repo::db_parser::{parse_repo_db, RepoPkgEntry, RepoDbContents};

/// Cached view of the repo.db.tar file.
///
/// Parsed on startup and re-parsed after repo-add/repo-remove operations
/// or when the file's mtime changes.
pub struct RepoState {
    contents: RepoDbContents,
    db_path: PathBuf,
    last_modified: Option<SystemTime>,
}

impl RepoState {
    /// Load (or initialize empty) repo state from the repository directory.
    ///
    /// Constructs the db path as `{repo_path}/{repo_name}.db.tar.gz` and parses
    /// it if the file exists. If the file doesn't exist yet, starts with empty state.
    pub fn load(repo_path: &Path, repo_name: &str) -> Result<Self> {
        let db_path = repo_path.join(format!("{}.db.tar.gz", repo_name));

        if db_path.exists() {
            let contents = parse_repo_db(&db_path)
                .with_context(|| format!("Failed to parse repo database: {}", db_path.display()))?;
            let last_modified = std::fs::metadata(&db_path)
                .ok()
                .and_then(|m| m.modified().ok());

            Ok(Self {
                contents,
                db_path,
                last_modified,
            })
        } else {
            Ok(Self {
                contents: RepoDbContents::default(),
                db_path,
                last_modified: None,
            })
        }
    }

    /// Re-parse the repo database file unconditionally.
    pub fn reload(&mut self) -> Result<()> {
        if self.db_path.exists() {
            self.contents = parse_repo_db(&self.db_path).with_context(|| {
                format!(
                    "Failed to parse repo database: {}",
                    self.db_path.display()
                )
            })?;
            self.last_modified = std::fs::metadata(&self.db_path)
                .ok()
                .and_then(|m| m.modified().ok());
        } else {
            self.contents = RepoDbContents::default();
            self.last_modified = None;
        }
        Ok(())
    }

    /// Check if the repo database file has been modified since last parse,
    /// and reload if so.
    ///
    /// Returns `true` if a reload was performed, `false` if the file is unchanged.
    pub fn reload_if_changed(&mut self) -> Result<bool> {
        let current_mtime = if self.db_path.exists() {
            std::fs::metadata(&self.db_path)
                .ok()
                .and_then(|m| m.modified().ok())
        } else {
            None
        };

        // Reload if:
        // - File appeared (was None, now Some)
        // - File disappeared (was Some, now None)
        // - mtime changed
        let changed = current_mtime != self.last_modified;

        if changed {
            self.reload()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Check if any package with this pkgbase is present in the repo.
    pub fn has_pkgbase(&self, pkgbase: &str) -> bool {
        self.contents.by_pkgbase.contains_key(pkgbase)
    }

    /// Get the repo version for a specific pkgname.
    pub fn get_version_for_pkgname(&self, pkgname: &str) -> Option<&str> {
        self.contents
            .packages
            .get(pkgname)
            .map(|entry| entry.version.as_str())
    }

    /// Get all repo entries for a given pkgbase.
    pub fn get_packages_for_pkgbase(&self, pkgbase: &str) -> Vec<&RepoPkgEntry> {
        self.contents
            .by_pkgbase
            .get(pkgbase)
            .map(|entries| entries.iter().collect())
            .unwrap_or_default()
    }

    /// Access the full parsed repo database contents.
    pub fn contents(&self) -> &RepoDbContents {
        &self.contents
    }
}
