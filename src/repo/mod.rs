pub mod db_parser;

use anyhow::{Context, Result};

use crate::build::vercmp;
use crate::config::Config;

/// Repository file management
pub struct RepoManager {
    config: Config,
}

impl RepoManager {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Get repository statistics
    pub fn stats(&self) -> Result<RepoStats> {
        let mut total_size = 0u64;
        let mut package_count = 0u32;
        let mut packages = Vec::new();

        for entry in std::fs::read_dir(&self.config.repo.path)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz") {
                    let metadata = entry.metadata()?;
                    total_size += metadata.len();
                    package_count += 1;

                    if let Some(pkg_info) = parse_package_filename(name) {
                        packages.push(pkg_info);
                    }
                }
            }
        }

        Ok(RepoStats {
            path: self.config.repo.path.clone(),
            name: self.config.repo.name.clone(),
            total_size,
            package_count,
            packages,
        })
    }

    /// Delete package files from repository
    pub fn delete_package_files(&self, pkgname: &str) -> Result<Vec<String>> {
        let mut deleted = Vec::new();

        for entry in std::fs::read_dir(&self.config.repo.path)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if (name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz"))
                    && package_name_matches(name, pkgname)
                {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("Failed to delete {}", path.display()))?;
                    deleted.push(name.to_string());
                }
            }
        }

        Ok(deleted)
    }

    /// List all package files for a given pkgname
    pub fn list_package_files(&self, pkgname: &str) -> Result<Vec<PackageFileInfo>> {
        let mut files = Vec::new();

        for entry in std::fs::read_dir(&self.config.repo.path)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if (name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz"))
                    && package_name_matches(name, pkgname)
                {
                    let metadata = entry.metadata()?;
                    if let Some(info) = parse_package_filename(name) {
                        files.push(PackageFileInfo {
                            filename: name.to_string(),
                            pkgname: info.pkgname,
                            version: info.version,
                            arch: info.arch,
                            size: metadata.len(),
                        });
                    }
                }
            }
        }

        // Sort by version (newest first) using vercmp
        files.sort_by(|a, b| vercmp(&b.version, &a.version));

        Ok(files)
    }

    /// Clean old package versions, keeping only the newest N
    pub fn clean_old_versions(&self, keep: u32) -> Result<Vec<String>> {
        let mut deleted = Vec::new();

        // Group packages by name
        let mut by_name: std::collections::HashMap<String, Vec<PackageFileInfo>> =
            std::collections::HashMap::new();

        for entry in std::fs::read_dir(&self.config.repo.path)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz") {
                    if let Some(info) = parse_package_filename(name) {
                        let metadata = entry.metadata()?;
                        by_name.entry(info.pkgname.clone()).or_default().push(
                            PackageFileInfo {
                                filename: name.to_string(),
                                pkgname: info.pkgname,
                                version: info.version,
                                arch: info.arch,
                                size: metadata.len(),
                            },
                        );
                    }
                }
            }
        }

        // For each package, delete all but the newest N versions
        for (_name, mut versions) in by_name {
            // Sort by version (newest first) using vercmp
            versions.sort_by(|a, b| vercmp(&b.version, &a.version));

            for old in versions.into_iter().skip(keep as usize) {
                let path = self.config.repo.path.join(&old.filename);
                std::fs::remove_file(&path)
                    .with_context(|| format!("Failed to delete {}", path.display()))?;
                deleted.push(old.filename);
            }
        }

        Ok(deleted)
    }

    /// Rebuild repository database from files
    pub fn rebuild_database(&self) -> Result<()> {
        let db_path = self
            .config
            .repo
            .path
            .join(format!("{}.db.tar.gz", self.config.repo.name));

        let files_db = self
            .config
            .repo
            .path
            .join(format!("{}.files.tar.gz", self.config.repo.name));

        // Find all packages
        let mut packages = Vec::new();
        for entry in std::fs::read_dir(&self.config.repo.path)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz") {
                    packages.push(path);
                }
            }
        }

        if packages.is_empty() {
            return Ok(());
        }

        // Write new DB to a temp file, then rename over the old one atomically
        let tmp_db_path = self
            .config
            .repo
            .path
            .join(format!("{}.db.tar.gz.tmp", self.config.repo.name));

        // Remove any leftover temp file
        if tmp_db_path.exists() {
            std::fs::remove_file(&tmp_db_path)?;
        }

        // Add all packages to the temp database
        let mut cmd = std::process::Command::new("repo-add");
        cmd.arg(&tmp_db_path);
        for pkg in &packages {
            cmd.arg(pkg);
        }

        let output = cmd.output().context("Failed to execute repo-add")?;

        if !output.status.success() {
            // Clean up temp file on failure
            let _ = std::fs::remove_file(&tmp_db_path);
            anyhow::bail!(
                "repo-add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Atomically replace the old database
        std::fs::rename(&tmp_db_path, &db_path)?;

        // Also remove .files database (repo-add regenerates it alongside the main db)
        if files_db.exists() {
            std::fs::remove_file(&files_db)?;
        }

        Ok(())
    }
}

/// Repository statistics
#[derive(Debug, serde::Serialize)]
pub struct RepoStats {
    pub path: std::path::PathBuf,
    pub name: String,
    pub total_size: u64,
    pub package_count: u32,
    pub packages: Vec<PackageBasicInfo>,
}

/// Basic package info parsed from filename
#[derive(Debug, Clone, serde::Serialize)]
pub struct PackageBasicInfo {
    pub pkgname: String,
    pub version: String,
    pub arch: String,
}

/// Detailed package file info
#[derive(Debug, Clone, serde::Serialize)]
pub struct PackageFileInfo {
    pub filename: String,
    pub pkgname: String,
    pub version: String,
    pub arch: String,
    pub size: u64,
}

/// Parse a package filename into components
/// Format: {pkgname}-{pkgver}-{pkgrel}-{arch}.pkg.tar.{zst|xz}
fn parse_package_filename(filename: &str) -> Option<PackageBasicInfo> {
    // Strip extension
    let name = filename
        .strip_suffix(".pkg.tar.zst")
        .or_else(|| filename.strip_suffix(".pkg.tar.xz"))?;

    // Split from the end to find arch, pkgrel, pkgver
    let parts: Vec<&str> = name.rsplitn(4, '-').collect();

    if parts.len() < 4 {
        return None;
    }

    // parts is reversed: [arch, pkgrel, pkgver, pkgname...]
    let arch = parts[0];
    let pkgrel = parts[1];
    let pkgver = parts[2];
    let pkgname = parts[3];

    Some(PackageBasicInfo {
        pkgname: pkgname.to_string(),
        version: format!("{}-{}", pkgver, pkgrel),
        arch: arch.to_string(),
    })
}

/// Check if a package filename matches a pkgname
fn package_name_matches(filename: &str, pkgname: &str) -> bool {
    parse_package_filename(filename)
        .map(|info| info.pkgname == pkgname)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_package_filename() {
        let info = parse_package_filename("example-1.0.0-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!(info.pkgname, "example");
        assert_eq!(info.version, "1.0.0-1");
        assert_eq!(info.arch, "x86_64");

        let info = parse_package_filename("qt6-base-6.7.0-1-x86_64.pkg.tar.zst").unwrap();
        assert_eq!(info.pkgname, "qt6-base");
        assert_eq!(info.version, "6.7.0-1");

        let info = parse_package_filename("lib32-mesa-24.0.0-1-x86_64.pkg.tar.xz").unwrap();
        assert_eq!(info.pkgname, "lib32-mesa");
        assert_eq!(info.version, "24.0.0-1");
    }
}
