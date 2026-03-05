use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::aur::read_srcinfo;
use crate::models::SrcInfo;

/// Cached .SRCINFO data with the commit it was parsed from
pub struct CachedSrcInfo {
    pub srcinfo: SrcInfo,
    /// The commit hash this .SRCINFO was parsed from
    pub commit: String,
}

/// In-memory cache of parsed .SRCINFO data from git clones.
///
/// Keyed by pkgbase. Populated lazily or bulk-loaded on startup.
/// Invalidated when a git fetch changes the HEAD commit.
pub struct SrcInfoCache {
    entries: HashMap<String, CachedSrcInfo>,
}

impl SrcInfoCache {
    /// Create an empty cache
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Look up cached .SRCINFO for a pkgbase
    pub fn get(&self, pkgbase: &str) -> Option<&CachedSrcInfo> {
        self.entries.get(pkgbase)
    }

    /// Add or update a cache entry
    pub fn insert(&mut self, pkgbase: String, srcinfo: SrcInfo, commit: String) {
        self.entries.insert(pkgbase, CachedSrcInfo { srcinfo, commit });
    }

    /// Remove a cache entry
    pub fn remove(&mut self, pkgbase: &str) {
        self.entries.remove(pkgbase);
    }

    /// Iterate over all cached entries. Needed for bulk status derivation and orphan detection.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &CachedSrcInfo)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Return the number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Remove the cached entry if the current commit doesn't match.
    ///
    /// This is used after a git fetch: if HEAD moved, the cached .SRCINFO
    /// is stale and should be re-parsed.
    pub fn invalidate_if_changed(&mut self, pkgbase: &str, current_commit: &str) {
        let should_remove = self
            .entries
            .get(pkgbase)
            .is_some_and(|cached| cached.commit != current_commit);
        if should_remove {
            self.entries.remove(pkgbase);
        }
    }

    /// Get the HEAD commit hash from a git clone directory.
    ///
    /// Returns `None` if the directory doesn't exist.
    /// Returns an error if the directory exists but `git rev-parse HEAD` fails.
    ///
    /// Note: This performs blocking I/O (shells out to git, reads files).
    /// Callers in async contexts should use `spawn_blocking` before acquiring write locks.
    pub fn get_head_commit(clone_dir: &Path) -> Result<Option<String>> {
        if !clone_dir.exists() {
            return Ok(None);
        }
        use crate::aur::AurGit;
        Ok(Some(AurGit::head_commit(clone_dir)?))
    }

    /// Read HEAD commit and parse .SRCINFO from a git clone directory.
    ///
    /// Returns `None` if the directory doesn't exist.
    /// Returns an error if the directory exists but reading/parsing fails.
    ///
    /// Note: This performs blocking I/O (shells out to git, reads files).
    /// Callers in async contexts should use `spawn_blocking` before acquiring write locks.
    pub fn load_from_clone(clone_dir: &Path) -> Result<Option<(SrcInfo, String)>> {
        if !clone_dir.exists() {
            return Ok(None);
        }

        let commit = Self::get_head_commit(clone_dir)?
            .context("Clone dir exists but HEAD commit could not be read")?;

        let srcinfo = read_srcinfo(clone_dir)
            .with_context(|| format!("Failed to parse .SRCINFO in {}", clone_dir.display()))?;

        Ok(Some((srcinfo, commit)))
    }

    /// Bulk-load .SRCINFO data for all tracked packages from their clone directories.
    ///
    /// Missing clones are logged as warnings but do not cause failure.
    pub fn bulk_load(&mut self, aur_clones_dir: &Path, pkgbases: &[String]) -> Result<()> {
        for pkgbase in pkgbases {
            let clone_dir = aur_clones_dir.join(pkgbase);
            match Self::load_from_clone(&clone_dir) {
                Ok(Some((srcinfo, commit))) => {
                    self.insert(pkgbase.clone(), srcinfo, commit);
                }
                Ok(None) => {
                    tracing::warn!(
                        "Clone directory missing for {}: {}",
                        pkgbase,
                        clone_dir.display()
                    );
                }
                Err(e) => {
                    tracing::warn!("Failed to load .SRCINFO for {}: {:#}", pkgbase, e);
                }
            }
        }
        Ok(())
    }
}
