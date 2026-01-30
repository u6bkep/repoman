use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Package status in the workflow
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum PackageStatus {
    /// Newly added, PKGBUILD needs review
    PendingReview,
    /// PKGBUILD approved, ready to build
    Approved,
    /// Currently building
    Building,
    /// Build failed
    Failed,
    /// Successfully built and in repo
    UpToDate,
    /// New upstream commit available
    UpdateAvailable,
    /// User rejected the PKGBUILD
    Rejected,
}

impl std::fmt::Display for PackageStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PendingReview => write!(f, "pending_review"),
            Self::Approved => write!(f, "approved"),
            Self::Building => write!(f, "building"),
            Self::Failed => write!(f, "failed"),
            Self::UpToDate => write!(f, "up_to_date"),
            Self::UpdateAvailable => write!(f, "update_available"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

/// Why a package was installed (like pacman's --asexplicit/--asdeps)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InstallReason {
    /// Package was explicitly added by the user
    #[default]
    Explicit,
    /// Package was added as a dependency of another package
    Dependency,
}

impl std::fmt::Display for InstallReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Explicit => write!(f, "explicit"),
            Self::Dependency => write!(f, "dependency"),
        }
    }
}

impl std::str::FromStr for InstallReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "explicit" => Ok(Self::Explicit),
            "dependency" => Ok(Self::Dependency),
            _ => Err(format!("Invalid install reason: {}", s)),
        }
    }
}

/// A tracked package (identified by pkgbase)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    /// The pkgbase (build unit identifier)
    pub pkgbase: String,
    /// Git clone URL
    pub aur_url: String,
    /// Local path to git clone
    pub local_path: String,
    /// Last approved commit hash
    pub approved_commit: Option<String>,
    /// Current HEAD commit hash
    pub current_commit: Option<String>,
    /// Package version from .SRCINFO
    pub pkgver: Option<String>,
    /// Current workflow status
    pub status: PackageStatus,
    /// Why this package was installed (explicit or dependency)
    pub install_reason: InstallReason,
    /// Auto-approve updates
    pub auto_approve: bool,
    /// Whether this package exists in AUR (false for removed/local packages)
    pub has_upstream: bool,
    /// Whether this package (or a package with same name) exists in official repos
    /// Used to detect packages that have migrated from AUR to official
    pub in_official_repos: bool,
    /// Reason for rejection (if status is Rejected)
    pub rejection_reason: Option<String>,
    /// When the package was added
    pub added_at: DateTime<Utc>,
    /// Last update time
    pub updated_at: DateTime<Utc>,
}

/// Individual package name produced by a pkgbase
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageName {
    /// The individual package name
    pub pkgname: String,
    /// The pkgbase this belongs to
    pub pkgbase: String,
    /// Package description
    pub pkgdesc: Option<String>,
    /// Package version (usually same as pkgbase)
    pub pkgver: Option<String>,
    /// Runtime dependencies (JSON array)
    pub depends: Option<String>,
    /// Optional dependencies (JSON array)
    pub optdepends: Option<String>,
    /// Packages this provides (JSON array)
    pub provides: Option<String>,
    /// Packages this conflicts with (JSON array)
    pub conflicts: Option<String>,
    /// Version currently in our repo
    pub repo_version: Option<String>,
}

/// Build status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum BuildStatus {
    /// Queued, waiting to start
    Pending,
    /// Currently running
    Running,
    /// Completed successfully
    Success,
    /// Build failed
    Failed,
    /// Cancelled by user
    Cancelled,
}

/// Current stage of a running build
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BuildStage {
    /// Build queued, not yet started
    #[default]
    Pending,
    /// Container starting up
    Starting,
    /// Updating package database (pacman -Sy)
    SyncingDb,
    /// Copying source files to build directory
    Preparing,
    /// Running makepkg (the main build phase)
    Building,
    /// Moving built packages to repository
    MovingToRepo,
    /// Updating repository index (repo-add)
    UpdatingIndex,
    /// Build completed successfully
    Complete,
    /// Build failed
    Failed,
}

impl std::fmt::Display for BuildStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Starting => write!(f, "starting"),
            Self::SyncingDb => write!(f, "syncing_db"),
            Self::Preparing => write!(f, "preparing"),
            Self::Building => write!(f, "building"),
            Self::MovingToRepo => write!(f, "moving_to_repo"),
            Self::UpdatingIndex => write!(f, "updating_index"),
            Self::Complete => write!(f, "complete"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

impl BuildStage {
    /// Parse a log line to detect stage changes
    /// Returns Some(new_stage) if the line indicates a stage change
    pub fn from_log_line(line: &str) -> Option<Self> {
        if line.contains("=== Updating package database ===") {
            Some(Self::SyncingDb)
        } else if line.contains("=== Copying source files ===") {
            Some(Self::Preparing)
        } else if line.contains("=== Starting build ===") {
            Some(Self::Building)
        } else if line.contains("=== Build complete ===") {
            Some(Self::Complete)
        } else {
            None
        }
    }
}

impl std::fmt::Display for BuildStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Success => write!(f, "success"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A build record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Build {
    /// Unique build ID
    pub id: String,
    /// The pkgbase being built
    pub pkgbase: String,
    /// Git commit being built
    pub commit: String,
    /// Build status
    pub status: BuildStatus,
    /// When the build started
    pub started_at: DateTime<Utc>,
    /// When the build finished
    pub finished_at: Option<DateTime<Utc>>,
    /// Path to build log file
    pub log_path: Option<String>,
    /// Built package filenames (JSON array)
    pub outputs: Option<String>,
    /// Error message if failed
    pub error_message: Option<String>,
}

/// Approval audit record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: i64,
    pub pkgbase: String,
    pub commit: String,
    pub approved_at: DateTime<Utc>,
    pub approved_by: Option<String>,
    pub auto_approved: bool,
}

/// AUR package info from RPC API
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AurPackageInfo {
    #[serde(rename = "ID")]
    pub id: u64,
    pub name: String,
    pub package_base: String,
    #[serde(rename = "PackageBaseID")]
    pub package_base_id: u64,
    pub version: String,
    pub description: Option<String>,
    #[serde(rename = "URL")]
    pub url: Option<String>,
    pub num_votes: u32,
    pub popularity: f64,
    pub out_of_date: Option<u64>,
    pub maintainer: Option<String>,
    pub first_submitted: u64,
    pub last_modified: u64,
}

/// AUR search result with tracking status
#[derive(Debug, Clone, Serialize)]
pub struct AurSearchResult {
    #[serde(flatten)]
    pub info: AurPackageInfo,
    /// Whether this package's pkgbase is already tracked by repoman
    pub tracked: bool,
}

/// Parsed .SRCINFO data
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SrcInfo {
    pub pkgbase: String,
    pub pkgver: String,
    pub pkgrel: String,
    pub epoch: Option<String>,
    pub pkgdesc: Option<String>,
    pub url: Option<String>,
    pub arch: Vec<String>,
    pub license: Vec<String>,
    pub depends: Vec<String>,
    pub makedepends: Vec<String>,
    pub checkdepends: Vec<String>,
    pub optdepends: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub source: Vec<String>,
    /// Checksums for sources (positionally matched with source array)
    pub sha256sums: Vec<String>,
    pub sha512sums: Vec<String>,
    pub b2sums: Vec<String>,
    pub md5sums: Vec<String>,
    /// Individual package sections
    pub packages: Vec<SrcInfoPackage>,
}

impl SrcInfo {
    /// Get the full version string (epoch:pkgver-pkgrel)
    pub fn full_version(&self) -> String {
        match &self.epoch {
            Some(e) => format!("{}:{}-{}", e, self.pkgver, self.pkgrel),
            None => format!("{}-{}", self.pkgver, self.pkgrel),
        }
    }

    /// Extract the filename from a source entry
    /// Handles renamed sources (filename::url) and plain URLs
    pub fn source_filename(source: &str) -> String {
        // Check for rename syntax: filename::url
        if let Some(idx) = source.find("::") {
            return source[..idx].to_string();
        }
        // Extract filename from URL or use as-is for local files
        source
            .rsplit('/')
            .next()
            .unwrap_or(source)
            .split('?') // Remove query params
            .next()
            .unwrap_or(source)
            .to_string()
    }

    /// Get preferred checksums (first non-empty checksum array)
    /// Returns (checksum_type, checksums) or None if no checksums
    pub fn preferred_checksums(&self) -> Option<(&'static str, &[String])> {
        if !self.b2sums.is_empty() {
            Some(("b2", &self.b2sums))
        } else if !self.sha512sums.is_empty() {
            Some(("sha512", &self.sha512sums))
        } else if !self.sha256sums.is_empty() {
            Some(("sha256", &self.sha256sums))
        } else if !self.md5sums.is_empty() {
            Some(("md5", &self.md5sums))
        } else {
            None
        }
    }

    /// Get source files with their checksums
    /// Returns Vec of (filename, checksum) pairs
    /// Checksum is None for SKIP entries or if no checksums defined
    pub fn sources_with_checksums(&self) -> Vec<(String, Option<String>)> {
        let checksums = self.preferred_checksums();
        self.source
            .iter()
            .enumerate()
            .map(|(i, src)| {
                let filename = Self::source_filename(src);
                let checksum = checksums
                    .and_then(|(_, sums)| sums.get(i))
                    .filter(|s| *s != "SKIP")
                    .cloned();
                (filename, checksum)
            })
            .collect()
    }
}

/// Per-package section in .SRCINFO
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SrcInfoPackage {
    pub pkgname: String,
    pub pkgdesc: Option<String>,
    pub depends: Vec<String>,
    pub optdepends: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
}
