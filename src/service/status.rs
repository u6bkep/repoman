use serde::Serialize;
use std::collections::HashSet;

use crate::models::{Build, BuildStatus};

/// Derived package status, computed at runtime from comparing real state sources.
///
/// Unlike the old `PackageStatus` which was stored in the database, this is always
/// computed fresh from the current state of the git clone, build history, and repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedStatus {
    /// Never approved -- PKGBUILD needs review
    PendingReview,
    /// Approved but not yet built (or last build was cancelled)
    Approved,
    /// Build currently in progress
    Building,
    /// Latest build of approved commit failed
    Failed,
    /// Built successfully and present in repo
    UpToDate,
    /// HEAD has moved past the approved commit
    UpdateAvailable,
    /// Clone directory does not exist (needs re-clone)
    Missing,
}

impl std::fmt::Display for DerivedStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PendingReview => write!(f, "pending_review"),
            Self::Approved => write!(f, "approved"),
            Self::Building => write!(f, "building"),
            Self::Failed => write!(f, "failed"),
            Self::UpToDate => write!(f, "up_to_date"),
            Self::UpdateAvailable => write!(f, "update_available"),
            Self::Missing => write!(f, "missing"),
        }
    }
}

/// Inputs needed to derive a package's status.
///
/// This struct takes pre-fetched data so the derivation logic is pure and testable.
/// The caller is responsible for gathering these inputs from the database, filesystem,
/// and in-memory state.
pub struct StatusInputs<'a> {
    /// The last commit hash that was approved (from DB `packages.approved_commit`)
    pub approved_commit: Option<&'a str>,
    /// The current HEAD commit of the git clone (None if clone directory is missing)
    pub head_commit: Option<&'a str>,
    /// The most recent build for the approved commit (if any)
    pub latest_build: Option<&'a Build>,
    /// Whether a build is currently running for this package (from in-memory active_builds set)
    pub is_active_build: bool,
    /// Whether any of this pkgbase's pkgnames are present in repo.db.tar.
    /// For split packages, this should be true if at least one pkgname is in the repo.
    pub in_repo: bool,
}

/// Derive a package's status from its current state.
///
/// Follows the V2 spec derivation logic exactly:
/// 1. If active build -> Building
/// 2. If no clone (head_commit is None) -> Missing
/// 3. If never approved (approved_commit is None) -> PendingReview
/// 4. If head != approved -> UpdateAvailable
/// 5. Check latest build for approved commit:
///    - None or Cancelled -> Approved
///    - Running or Pending -> Building
///    - Failed -> Failed
///    - Success + in_repo -> UpToDate
///    - Success + not in_repo -> Approved (repo-add may have failed)
/// 6. Fallback -> Approved
pub fn derive_status(inputs: &StatusInputs) -> DerivedStatus {
    // 1. Active build takes priority
    if inputs.is_active_build {
        return DerivedStatus::Building;
    }

    // 2. Clone missing
    let head_commit = match inputs.head_commit {
        Some(h) => h,
        None => return DerivedStatus::Missing,
    };

    // 3. Never approved
    let approved_commit = match inputs.approved_commit {
        Some(a) => a,
        None => return DerivedStatus::PendingReview,
    };

    // 4. HEAD moved past approved commit
    if head_commit != approved_commit {
        return DerivedStatus::UpdateAvailable;
    }

    // 5. Check latest build for the approved commit
    match inputs.latest_build {
        None => DerivedStatus::Approved,
        Some(build) => {
            debug_assert!(
                build.commit == approved_commit,
                "latest_build commit ({}) should match approved_commit ({})",
                build.commit, approved_commit
            );
            match build.status {
            BuildStatus::Cancelled => DerivedStatus::Approved,
            BuildStatus::Running | BuildStatus::Pending => DerivedStatus::Building,
            BuildStatus::Failed => DerivedStatus::Failed,
            BuildStatus::Success => {
                if inputs.in_repo {
                    DerivedStatus::UpToDate
                } else {
                    // Built successfully but not in repo -- repo-add may have failed
                    DerivedStatus::Approved
                }
            }
        }
        },
    }
}

/// Check if a package is an orphan.
///
/// A package is an orphan if:
/// - Its install_reason is Dependency (caller ensures this)
/// - No explicitly-installed package depends on any of its pkgnames
///
/// This takes pre-computed sets to avoid recomputation in bulk calls.
///
/// # Arguments
/// * `pkgnames` - The pkgnames produced by this package (from .SRCINFO)
/// * `explicit_deps` - All dependency names (version-stripped) of all explicitly-installed packages
pub fn is_orphan(pkgnames: &[String], explicit_deps: &HashSet<String>) -> bool {
    !pkgnames
        .iter()
        .any(|name| explicit_deps.contains(name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Build, BuildStatus};
    use chrono::Utc;

    fn make_build(status: BuildStatus, commit: &str) -> Build {
        Build {
            id: "test-build".to_string(),
            pkgbase: "test".to_string(),
            commit: commit.to_string(),
            status,
            started_at: Utc::now(),
            finished_at: None,
            log_path: None,
            outputs: None,
            error_message: None,
        }
    }

    #[test]
    fn test_active_build_returns_building() {
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: None,
            is_active_build: true,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Building);
    }

    #[test]
    fn test_active_build_takes_priority_over_everything() {
        // Even if there's a failed build, active build wins
        let build = make_build(BuildStatus::Failed, "abc123");
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: Some(&build),
            is_active_build: true,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Building);
    }

    #[test]
    fn test_active_build_takes_priority_over_missing_clone() {
        let inputs = StatusInputs {
            approved_commit: None,
            head_commit: None,  // clone missing
            latest_build: None,
            is_active_build: true,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Building);
    }

    #[test]
    fn test_missing_clone_returns_missing() {
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: None,
            latest_build: None,
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Missing);
    }

    #[test]
    fn test_never_approved_returns_pending_review() {
        let inputs = StatusInputs {
            approved_commit: None,
            head_commit: Some("abc123"),
            latest_build: None,
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::PendingReview);
    }

    #[test]
    fn test_head_moved_returns_update_available() {
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("def456"),
            latest_build: None,
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::UpdateAvailable);
    }

    #[test]
    fn test_no_builds_returns_approved() {
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: None,
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Approved);
    }

    #[test]
    fn test_cancelled_build_returns_approved() {
        let build = make_build(BuildStatus::Cancelled, "abc123");
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: Some(&build),
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Approved);
    }

    #[test]
    fn test_running_build_returns_building() {
        let build = make_build(BuildStatus::Running, "abc123");
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: Some(&build),
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Building);
    }

    #[test]
    fn test_pending_build_returns_building() {
        let build = make_build(BuildStatus::Pending, "abc123");
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: Some(&build),
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Building);
    }

    #[test]
    fn test_failed_build_returns_failed() {
        let build = make_build(BuildStatus::Failed, "abc123");
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: Some(&build),
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Failed);
    }

    #[test]
    fn test_success_and_in_repo_returns_up_to_date() {
        let build = make_build(BuildStatus::Success, "abc123");
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: Some(&build),
            is_active_build: false,
            in_repo: true,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::UpToDate);
    }

    #[test]
    fn test_success_but_not_in_repo_returns_approved() {
        let build = make_build(BuildStatus::Success, "abc123");
        let inputs = StatusInputs {
            approved_commit: Some("abc123"),
            head_commit: Some("abc123"),
            latest_build: Some(&build),
            is_active_build: false,
            in_repo: false,
        };
        assert_eq!(derive_status(&inputs), DerivedStatus::Approved);
    }

    #[test]
    fn test_display_formatting() {
        assert_eq!(DerivedStatus::PendingReview.to_string(), "pending_review");
        assert_eq!(DerivedStatus::Approved.to_string(), "approved");
        assert_eq!(DerivedStatus::Building.to_string(), "building");
        assert_eq!(DerivedStatus::Failed.to_string(), "failed");
        assert_eq!(DerivedStatus::UpToDate.to_string(), "up_to_date");
        assert_eq!(
            DerivedStatus::UpdateAvailable.to_string(),
            "update_available"
        );
        assert_eq!(DerivedStatus::Missing.to_string(), "missing");
    }

    #[test]
    fn test_is_orphan_with_no_dependents() {
        let pkgnames = vec!["libfoo".to_string()];
        let explicit_deps: HashSet<String> =
            ["glibc", "openssl"].iter().map(|s| s.to_string()).collect();
        assert!(is_orphan(&pkgnames, &explicit_deps));
    }

    #[test]
    fn test_is_orphan_with_dependents() {
        let pkgnames = vec!["libfoo".to_string()];
        let explicit_deps: HashSet<String> = ["glibc", "libfoo", "openssl"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(!is_orphan(&pkgnames, &explicit_deps));
    }

    #[test]
    fn test_is_orphan_multiple_pkgnames_one_depended() {
        let pkgnames = vec!["libfoo".to_string(), "libfoo-dev".to_string()];
        let explicit_deps: HashSet<String> = ["libfoo-dev"].iter().map(|s| s.to_string()).collect();
        assert!(!is_orphan(&pkgnames, &explicit_deps));
    }

    #[test]
    fn test_is_orphan_empty_pkgnames() {
        let pkgnames: Vec<String> = vec![];
        let explicit_deps: HashSet<String> = ["glibc"].iter().map(|s| s.to_string()).collect();
        assert!(is_orphan(&pkgnames, &explicit_deps));
    }

    #[test]
    fn test_is_orphan_empty_deps() {
        let pkgnames = vec!["libfoo".to_string()];
        let explicit_deps: HashSet<String> = HashSet::new();
        assert!(is_orphan(&pkgnames, &explicit_deps));
    }
}
