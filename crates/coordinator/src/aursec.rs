//! aur-security (aur-security.cretezy.com) lookups: LLM assessments of AUR
//! commits published by a third party. They are signals in the plan's
//! sense: stored with the commit they concern, shown on the candidate, and
//! blocking for auto-approval when negative; never approving anything.
//!
//! Wire format observed 2026-11-01: `POST /api/v1/checks/lookup` with
//! `{"packages":[{"package_base":..,"commits":[..]}]}` answers
//! `{"results":[{"package_base":..,"commits":[{"commit":..,"assessment":null|{..}}]}]}`.
//! The assessment object carries a `verdict` of safe, suspicious, or
//! dangerous plus an explanation; it is stored whole.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

pub struct AurSecurity {
    http: reqwest::Client,
    base: String,
}

#[derive(Debug, Deserialize)]
struct LookupResponse {
    results: Vec<LookupResult>,
}

#[derive(Debug, Deserialize)]
struct LookupResult {
    package_base: String,
    commits: Vec<CommitAssessment>,
}

#[derive(Debug, Deserialize)]
pub struct CommitAssessment {
    pub commit: String,
    pub assessment: Option<serde_json::Value>,
}

/// Signal severity for an assessment: dangerous → high, suspicious →
/// medium, safe (or anything unrecognised) → info.
pub fn severity(assessment: &serde_json::Value) -> &'static str {
    let v = assessment["verdict"].as_str().or_else(|| assessment["result"].as_str()).unwrap_or("").to_ascii_lowercase();
    match v.as_str() {
        "dangerous" | "malicious" => "high",
        "suspicious" => "medium",
        _ => "info",
    }
}

impl AurSecurity {
    pub fn new(base: &str) -> Self {
        AurSecurity {
            http: reqwest::Client::builder()
                .user_agent(format!("repoman/{}", repoman_common::VERSION))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// Assessments for `commits` of `pkgbase` (at most 100 per call).
    pub async fn lookup(&self, pkgbase: &str, commits: &[String]) -> Result<Vec<CommitAssessment>> {
        if commits.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({"packages": [{"package_base": pkgbase, "commits": &commits[..commits.len().min(100)]}]});
        let resp = self
            .http
            .post(format!("{}/api/v1/checks/lookup", self.base))
            .json(&body)
            .send()
            .await
            .context("aur-security request")?
            .error_for_status()
            .context("aur-security status")?;
        let parsed: LookupResponse = resp.json().await.context("aur-security body")?;
        Ok(parsed
            .results
            .into_iter()
            .filter(|r| r.package_base == pkgbase)
            .flat_map(|r| r.commits)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severities() {
        assert_eq!(severity(&serde_json::json!({"verdict": "dangerous"})), "high");
        assert_eq!(severity(&serde_json::json!({"verdict": "Suspicious"})), "medium");
        assert_eq!(severity(&serde_json::json!({"verdict": "safe"})), "info");
        assert_eq!(severity(&serde_json::json!({})), "info");
    }
}
