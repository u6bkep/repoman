use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

use crate::models::{AurPackageInfo, SrcInfo, SrcInfoPackage};

const AUR_RPC_URL: &str = "https://aur.archlinux.org/rpc/v5";
const AUR_GIT_URL: &str = "https://aur.archlinux.org";

/// AUR RPC API client
pub struct AurClient {
    client: reqwest::Client,
}

impl AurClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to build reqwest client"),
        }
    }

    /// Search for packages in AUR
    pub async fn search(&self, query: &str) -> Result<Vec<AurPackageInfo>> {
        let url = format!("{}/search/{}", AUR_RPC_URL, urlencoding::encode(query));
        let response: AurResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to send AUR search request")?
            .json()
            .await
            .context("Failed to parse AUR search response")?;

        if response.r#type == "error" {
            anyhow::bail!("AUR error: {}", response.error.unwrap_or_default());
        }

        Ok(response.results.unwrap_or_default())
    }

    /// Get info for specific packages
    pub async fn info(&self, packages: &[&str]) -> Result<Vec<AurPackageInfo>> {
        if packages.is_empty() {
            return Ok(vec![]);
        }

        let params: Vec<_> = packages.iter().map(|p| ("arg[]", *p)).collect();
        let url = format!("{}/info", AUR_RPC_URL);

        let response: AurResponse = self
            .client
            .get(&url)
            .query(&params)
            .send()
            .await
            .context("Failed to send AUR info request")?
            .json()
            .await
            .context("Failed to parse AUR info response")?;

        if response.r#type == "error" {
            anyhow::bail!("AUR error: {}", response.error.unwrap_or_default());
        }

        Ok(response.results.unwrap_or_default())
    }

    /// Get info for a single package
    pub async fn info_single(&self, package: &str) -> Result<Option<AurPackageInfo>> {
        let results = self.info(&[package]).await?;
        Ok(results.into_iter().next())
    }

    /// Find missing AUR dependencies for a package
    /// Returns deps that are:
    /// - Not in official repos or our local repo
    /// - Not already tracked by us
    /// - Exist in AUR (with their info)
    ///
    /// Uses the container image to check deps, so it sees the same repos as builds.
    pub async fn find_missing_aur_deps(
        &self,
        srcinfo: &SrcInfo,
        tracked_pkgnames: &std::collections::HashSet<String>,
        container_image: &str,
    ) -> Result<Vec<MissingDep>> {
        // Get all dependencies
        let all_deps = DepResolver::get_all_deps(srcinfo);
        if all_deps.is_empty() {
            return Ok(vec![]);
        }

        // Filter out deps satisfied by repos (official + our local repo, checked in container)
        let foreign_deps = DepResolver::filter_foreign_deps(&all_deps, container_image)?;
        if foreign_deps.is_empty() {
            return Ok(vec![]);
        }

        // Filter out deps we already track (by pkgname)
        let untracked: Vec<_> = foreign_deps
            .into_iter()
            .filter(|dep| {
                let name = DepResolver::dep_name(dep);
                !tracked_pkgnames.contains(name)
            })
            .collect();

        if untracked.is_empty() {
            return Ok(vec![]);
        }

        // Query AUR for the untracked deps
        let dep_names: Vec<&str> = untracked
            .iter()
            .map(|d| DepResolver::dep_name(d))
            .collect();

        let aur_results = self.info(&dep_names).await?;

        // Build a map of name -> AUR info
        let aur_map: std::collections::HashMap<&str, &AurPackageInfo> = aur_results
            .iter()
            .map(|info| (info.name.as_str(), info))
            .collect();

        // Build the result
        let mut missing = Vec::new();
        for dep in untracked {
            let name = DepResolver::dep_name(&dep).to_string();
            let aur_info = aur_map.get(name.as_str());

            missing.push(MissingDep {
                dep: dep.clone(),
                name: name.clone(),
                in_aur: aur_info.is_some(),
                pkgbase: aur_info.map(|i| i.package_base.clone()),
            });
        }

        Ok(missing)
    }
}

impl Default for AurClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(serde::Deserialize)]
struct AurResponse {
    #[serde(rename = "type")]
    r#type: String,
    results: Option<Vec<AurPackageInfo>>,
    error: Option<String>,
}

/// Git operations for AUR packages
pub struct AurGit;

impl AurGit {
    /// Clone an AUR package repository
    pub fn clone(pkgbase: &str, dest: &Path) -> Result<()> {
        let url = format!("{}/{}.git", AUR_GIT_URL, pkgbase);

        let output = Command::new("git")
            .args(["clone", &url])
            .arg(dest)
            // Abort if transfer speed drops below 1000 bytes/sec for 30 seconds
            .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
            .env("GIT_HTTP_LOW_SPEED_TIME", "30")
            .output()
            .with_context(|| format!("Failed to execute 'git clone' for package '{}'", pkgbase))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not found") || stderr.contains("does not exist") {
                anyhow::bail!("AUR package '{}' not found at {}", pkgbase, url);
            }
            anyhow::bail!(
                "Failed to clone AUR package '{}': {}",
                pkgbase,
                stderr.trim()
            );
        }

        Ok(())
    }

    /// Fetch updates for an AUR package
    pub fn fetch(repo_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(repo_path)
            // Abort if transfer speed drops below 1000 bytes/sec for 30 seconds
            .env("GIT_HTTP_LOW_SPEED_LIMIT", "1000")
            .env("GIT_HTTP_LOW_SPEED_TIME", "30")
            .output()
            .context("Failed to execute git fetch")?;

        if !output.status.success() {
            anyhow::bail!(
                "git fetch failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Get the current HEAD commit hash
    pub fn head_commit(repo_path: &Path) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_path)
            .output()
            .context("Failed to execute git rev-parse")?;

        if !output.status.success() {
            anyhow::bail!(
                "git rev-parse failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Get the remote HEAD commit hash (origin/master or origin/main)
    pub fn remote_head_commit(repo_path: &Path) -> Result<String> {
        // Try origin/master first, then origin/main
        for branch in &["origin/master", "origin/main"] {
            let output = Command::new("git")
                .args(["rev-parse", branch])
                .current_dir(repo_path)
                .output()
                .context("Failed to execute git rev-parse")?;

            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
        }

        anyhow::bail!("Could not find origin/master or origin/main")
    }

    /// Reset to remote HEAD
    pub fn reset_to_remote(repo_path: &Path) -> Result<()> {
        // Try origin/master first, then origin/main
        for branch in &["origin/master", "origin/main"] {
            let output = Command::new("git")
                .args(["reset", "--hard", branch])
                .current_dir(repo_path)
                .output()
                .context("Failed to execute git reset")?;

            if output.status.success() {
                return Ok(());
            }
        }

        anyhow::bail!("Could not reset to origin/master or origin/main")
    }

    /// Get diff between two commits
    pub fn diff(repo_path: &Path, from: &str, to: &str) -> Result<String> {
        // Validate commit hashes to prevent flag injection
        let is_valid_hash = |s: &str| {
            s.len() >= 7
                && s.len() <= 64
                && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        };
        if !is_valid_hash(from) {
            anyhow::bail!("Invalid commit hash '{}': must be 7-64 lowercase hex characters", from);
        }
        if !is_valid_hash(to) {
            anyhow::bail!("Invalid commit hash '{}': must be 7-64 lowercase hex characters", to);
        }

        let output = Command::new("git")
            .args(["diff", from, to])
            .current_dir(repo_path)
            .output()
            .context("Failed to execute git diff")?;

        if !output.status.success() {
            anyhow::bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Check if repository has updates available
    pub fn has_updates(repo_path: &Path) -> Result<bool> {
        let local = Self::head_commit(repo_path)?;
        Self::fetch(repo_path)?;
        let remote = Self::remote_head_commit(repo_path)?;
        Ok(local != remote)
    }
}

/// Parse a .SRCINFO file
pub fn parse_srcinfo(content: &str) -> Result<SrcInfo> {
    let mut info = SrcInfo::default();
    let mut current_package: Option<SrcInfoPackage> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on first '='
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };

        let key = key.trim();
        let value = value.trim();

        // Check if we're starting a new package section
        if key == "pkgname" {
            // Save previous package if any
            if let Some(pkg) = current_package.take() {
                info.packages.push(pkg);
            }
            current_package = Some(SrcInfoPackage {
                pkgname: value.to_string(),
                ..Default::default()
            });
            continue;
        }

        // If we're in a package section, some keys are package-specific
        if let Some(ref mut pkg) = current_package {
            match key {
                "pkgdesc" => pkg.pkgdesc = Some(value.to_string()),
                "depends" => pkg.depends.push(value.to_string()),
                "optdepends" => pkg.optdepends.push(value.to_string()),
                "provides" => pkg.provides.push(value.to_string()),
                "conflicts" => pkg.conflicts.push(value.to_string()),
                _ => {} // Other keys in package section are ignored
            }
        } else {
            // Global section (before first pkgname)
            match key {
                "pkgbase" => info.pkgbase = value.to_string(),
                "pkgver" => info.pkgver = value.to_string(),
                "pkgrel" => info.pkgrel = value.to_string(),
                "epoch" => info.epoch = Some(value.to_string()),
                "pkgdesc" => info.pkgdesc = Some(value.to_string()),
                "url" => info.url = Some(value.to_string()),
                "arch" => info.arch.push(value.to_string()),
                "license" => info.license.push(value.to_string()),
                "depends" => info.depends.push(value.to_string()),
                "makedepends" => info.makedepends.push(value.to_string()),
                "checkdepends" => info.checkdepends.push(value.to_string()),
                "optdepends" => info.optdepends.push(value.to_string()),
                "provides" => info.provides.push(value.to_string()),
                "conflicts" => info.conflicts.push(value.to_string()),
                "source" => info.source.push(value.to_string()),
                "sha256sums" => info.sha256sums.push(value.to_string()),
                "sha512sums" => info.sha512sums.push(value.to_string()),
                "b2sums" => info.b2sums.push(value.to_string()),
                "md5sums" => info.md5sums.push(value.to_string()),
                // Also handle arch-specific sources/checksums (e.g., source_x86_64)
                k if k.starts_with("source_") => info.source.push(value.to_string()),
                k if k.starts_with("sha256sums_") => info.sha256sums.push(value.to_string()),
                k if k.starts_with("sha512sums_") => info.sha512sums.push(value.to_string()),
                k if k.starts_with("b2sums_") => info.b2sums.push(value.to_string()),
                k if k.starts_with("md5sums_") => info.md5sums.push(value.to_string()),
                _ => {}
            }
        }
    }

    // Don't forget the last package
    if let Some(pkg) = current_package {
        info.packages.push(pkg);
    }

    Ok(info)
}

/// Read and parse .SRCINFO from a repository
pub fn read_srcinfo(repo_path: &Path) -> Result<SrcInfo> {
    let srcinfo_path = repo_path.join(".SRCINFO");
    let content = std::fs::read_to_string(&srcinfo_path)
        .with_context(|| format!("Failed to read .SRCINFO from {}", srcinfo_path.display()))?;
    parse_srcinfo(&content)
}

/// Dependency resolution utilities
pub struct DepResolver;

impl DepResolver {
    /// Extract the package name from a dependency string (strips version constraints)
    /// e.g., "python>=3.10" -> "python"
    pub fn dep_name(dep: &str) -> &str {
        // Find first occurrence of <, >, =, :
        dep.split(|c| c == '<' || c == '>' || c == '=' || c == ':')
            .next()
            .unwrap_or(dep)
    }

    /// Check if dependencies are satisfied by repos (official + our local repo)
    /// Runs inside a single podman container to use the correct pacman configuration.
    /// Returns the list of deps NOT satisfied by any configured repos.
    pub fn filter_foreign_deps(deps: &[String], container_image: &str) -> Result<Vec<String>> {
        if deps.is_empty() {
            return Ok(vec![]);
        }

        // Run all deps in a single container using pacman -Sddp
        // -Sddp: sync, skip all deps, print-only (doesn't install, just checks availability)
        // When a dep is not found, pacman prints to stderr: "error: target not found: <name>"
        let dep_names: Vec<&str> = deps.iter().map(|d| Self::dep_name(d)).collect();

        let mut cmd = Command::new("podman");
        cmd.args(["run", "--rm", "-i", container_image, "pacman", "-Sddp", "--noconfirm", "--"]);
        cmd.args(&dep_names);

        let output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                // If we can't start the container at all, treat all deps as unknown (not foreign)
                // rather than crashing the whole add operation.
                eprintln!("Warning: failed to start podman for dep check: {e}");
                return Ok(vec![]);
            }
        };

        // Parse stderr for "error: target not found: <name>" messages
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut foreign = Vec::new();
        for line in stderr.lines() {
            if let Some(rest) = line.strip_prefix("error: target not found: ") {
                let not_found_name = rest.trim();
                // Find the original dep string (which may have version constraints) that
                // corresponds to this package name.
                if let Some(dep) = deps.iter().find(|d| Self::dep_name(d) == not_found_name) {
                    foreign.push(dep.clone());
                }
            }
        }

        Ok(foreign)
    }

    /// Get all dependencies from a SrcInfo (depends + makedepends + checkdepends)
    pub fn get_all_deps(srcinfo: &SrcInfo) -> Vec<String> {
        let mut deps = Vec::new();
        deps.extend(srcinfo.depends.clone());
        deps.extend(srcinfo.makedepends.clone());
        deps.extend(srcinfo.checkdepends.clone());

        // Also include per-package depends
        for pkg in &srcinfo.packages {
            deps.extend(pkg.depends.clone());
        }

        // Deduplicate
        deps.sort();
        deps.dedup();
        deps
    }
}

/// Information about missing AUR dependencies
#[derive(Debug, Clone, serde::Serialize)]
pub struct MissingDep {
    /// The dependency string as specified (may include version constraint)
    pub dep: String,
    /// The package name (without version constraint)
    pub name: String,
    /// Whether this package exists in AUR
    pub in_aur: bool,
    /// The pkgbase if it exists in AUR
    pub pkgbase: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_srcinfo() {
        let content = r#"
pkgbase = example
	pkgver = 1.0.0
	pkgrel = 1
	pkgdesc = An example package
	arch = x86_64
	license = MIT
	makedepends = cmake
	depends = glibc

pkgname = example
	pkgdesc = Main example package

pkgname = example-debug
	pkgdesc = Debug symbols for example
	depends = example
"#;

        let info = parse_srcinfo(content).unwrap();
        assert_eq!(info.pkgbase, "example");
        assert_eq!(info.pkgver, "1.0.0");
        assert_eq!(info.packages.len(), 2);
        assert_eq!(info.packages[0].pkgname, "example");
        assert_eq!(info.packages[1].pkgname, "example-debug");
    }
}
