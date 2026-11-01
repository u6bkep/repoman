//! Corroboration of routine updates against the upstream forge: does the
//! version the PKGBUILD now fetches exist as a tag there, under the same
//! owner the baseline fetched from? A missing tag or a different owner is
//! not proof of anything, but it is exactly what a hijacked source looks
//! like, so it raises the floor risk to medium and the operator decides.
//!
//! GitHub, GitLab, and Codeberg (Forgejo) are understood. Anything else is
//! reported as `unconfirmed` and changes nothing.

use std::time::Duration;

use repoman_alpm::srcinfo::Srcinfo;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Forge {
    GitHub,
    GitLab,
    Codeberg,
}

/// A source entry that points at a forge, with the version string the URL
/// embeds (tag or release name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeSource {
    pub forge: Forge,
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// Tag or ref as written in the URL, e.g. `v1.2.3`, when recognisable.
    pub tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Result_ {
    /// Tag exists under the same owner as the baseline.
    Confirmed,
    /// Not a forge we know, or no version in the URL, or the API was unreachable.
    Unconfirmed,
    /// The forge answered and the tag is not there, or the owner changed.
    Mismatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Corroboration {
    pub result: Result_,
    pub detail: String,
    pub sources: Vec<ForgeSource>,
}

fn url_of(entry: &str) -> &str {
    let e = entry.split_once("::").map(|(_, u)| u).unwrap_or(entry);
    e.strip_prefix("git+").unwrap_or(e)
}

/// Pull a tag-looking token out of a forge URL: `/archive/refs/tags/v1.2.tar.gz`,
/// `/archive/v1.2.tar.gz`, `/releases/download/v1.2/x`, `/-/archive/v1.2/...`,
/// `#tag=v1.2`.
fn tag_in_url(path: &str, fragment: Option<&str>) -> Option<String> {
    if let Some(f) = fragment
        && let Some(t) = f.strip_prefix("tag=")
    {
        return Some(t.to_string());
    }
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let strip_archive = |s: &str| -> String {
        let mut s = s.to_string();
        for ext in [".tar.gz", ".tar.bz2", ".tar.xz", ".tgz", ".zip", ".tar.zst", ".tar"] {
            if let Some(b) = s.strip_suffix(ext) {
                s = b.to_string();
                break;
            }
        }
        s
    };
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "archive" => {
                // github: archive/refs/tags/<tag>.tar.gz or archive/<tag>.tar.gz
                // gitlab: -/archive/<tag>/<repo>-<tag>.tar.gz
                let rest = &segs[i + 1..];
                let tag = match rest {
                    ["refs", "tags", t, ..] => Some(strip_archive(t)),
                    [t, ..] => Some(strip_archive(t)),
                    [] => None,
                };
                return tag.filter(|t| !t.is_empty());
            }
            "download" if i > 0 && segs[i - 1] == "releases" => {
                return segs.get(i + 1).map(|t| t.to_string());
            }
            _ => {}
        }
    }
    None
}

/// Recognise the forge sources in a SRCINFO (base section, all arches).
pub fn forge_sources(si: &Srcinfo) -> Vec<ForgeSource> {
    let mut out = Vec::new();
    for (k, v) in &si.base.fields {
        if !(k == "source" || k.starts_with("source_")) {
            continue;
        }
        let url = url_of(v);
        let (url, fragment) = match url.split_once('#') {
            Some((u, f)) => (u, Some(f)),
            None => (url, None),
        };
        let Some((_, rest)) = url.split_once("://") else {
            continue;
        };
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = host.to_ascii_lowercase();
        let forge = match host.as_str() {
            "github.com" | "www.github.com" | "codeload.github.com" => Forge::GitHub,
            "gitlab.com" | "www.gitlab.com" => Forge::GitLab,
            "codeberg.org" => Forge::Codeberg,
            _ => continue,
        };
        let mut segs = path.split('/').filter(|s| !s.is_empty());
        let (Some(owner), Some(repo)) = (segs.next(), segs.next()) else {
            continue;
        };
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        let tag = tag_in_url(path, fragment);
        let src = ForgeSource {
            forge,
            host,
            owner: owner.to_string(),
            repo: repo.to_string(),
            tag,
        };
        if !out.contains(&src) {
            out.push(src);
        }
    }
    out
}

pub struct ForgeClient {
    http: reqwest::Client,
    github_token: Option<String>,
}

impl ForgeClient {
    pub fn new(github_token: Option<String>) -> Self {
        ForgeClient {
            http: reqwest::Client::builder()
                .user_agent(format!("repoman/{}", repoman_common::VERSION))
                .timeout(Duration::from_secs(20))
                .build()
                .expect("http client"),
            github_token,
        }
    }

    /// Does `tag` exist in the repository? `None` when the forge could not
    /// be asked (network, rate limit, 5xx).
    async fn tag_exists(&self, s: &ForgeSource, tag: &str) -> Option<bool> {
        let enc = |x: &str| x.replace('/', "%2F");
        let url = match s.forge {
            Forge::GitHub => format!("https://api.github.com/repos/{}/{}/git/ref/tags/{}", s.owner, s.repo, tag),
            Forge::GitLab => format!("https://gitlab.com/api/v4/projects/{}/repository/tags/{}", enc(&format!("{}/{}", s.owner, s.repo)), enc(tag)),
            Forge::Codeberg => format!("https://codeberg.org/api/v1/repos/{}/{}/tags/{}", s.owner, s.repo, tag),
        };
        let mut req = self.http.get(&url).header("accept", "application/json");
        if s.forge == Forge::GitHub
            && let Some(t) = &self.github_token
        {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => Some(true),
            Ok(r) if r.status().as_u16() == 404 => Some(false),
            Ok(r) => {
                tracing::warn!(%url, status = %r.status(), "forge query failed");
                None
            }
            Err(e) => {
                tracing::warn!(%url, error = %e, "forge query failed");
                None
            }
        }
    }

    /// Check the head's forge sources against the baseline's.
    pub async fn corroborate(&self, baseline: Option<&Srcinfo>, head: &Srcinfo) -> Corroboration {
        let sources = forge_sources(head);
        if sources.is_empty() {
            return Corroboration {
                result: Result_::Unconfirmed,
                detail: "no source on a known forge".into(),
                sources,
            };
        }
        let before = baseline.map(forge_sources).unwrap_or_default();
        let mut details = Vec::new();
        let mut result = Result_::Unconfirmed;
        let mut any_confirmed = false;
        for s in &sources {
            let id = format!("{}/{}/{}", s.host, s.owner, s.repo);
            // Owner change on the same forge is the typosquat signature.
            if !before.is_empty() && !before.iter().any(|b| b.host == s.host && b.owner.eq_ignore_ascii_case(&s.owner)) {
                let was: Vec<String> = before.iter().map(|b| format!("{}/{}", b.host, b.owner)).collect();
                details.push(format!("{id}: owner differs from baseline {was:?}"));
                result = Result_::Mismatch;
                continue;
            }
            let Some(tag) = &s.tag else {
                details.push(format!("{id}: no tag in the source URL"));
                continue;
            };
            match self.tag_exists(s, tag).await {
                Some(true) => {
                    details.push(format!("{id}: tag {tag} exists"));
                    any_confirmed = true;
                }
                Some(false) => {
                    details.push(format!("{id}: tag {tag} not found"));
                    result = Result_::Mismatch;
                }
                None => details.push(format!("{id}: forge not reachable")),
            }
        }
        if result != Result_::Mismatch && any_confirmed {
            result = Result_::Confirmed;
        }
        Corroboration {
            result,
            detail: details.join("; "),
            sources,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn si(sources: &[&str]) -> Srcinfo {
        let s: String = sources.iter().map(|x| format!("\tsource = {x}\n")).collect();
        Srcinfo::parse(&format!("pkgbase = x\n\tpkgver = 1\n\tpkgrel = 1\n{s}pkgname = x\n")).unwrap()
    }

    #[test]
    fn recognises_forge_urls_and_tags() {
        let f = forge_sources(&si(&[
            "x-1.2.3.tar.gz::https://github.com/Owner/Repo/archive/refs/tags/v1.2.3.tar.gz",
            "https://github.com/owner/other/archive/1.2.3.tar.gz",
            "https://github.com/owner/bin/releases/download/v1.2.3/bin-linux-amd64",
            "https://gitlab.com/grp/proj/-/archive/v1.2.3/proj-v1.2.3.tar.bz2",
            "x::git+https://codeberg.org/o/r.git#tag=v1.2.3",
            "https://example.org/x-1.2.3.tar.gz",
            "x.patch",
        ]));
        let tags: Vec<(&str, &str, Option<&str>)> = f.iter().map(|s| (s.host.as_str(), s.repo.as_str(), s.tag.as_deref())).collect();
        assert_eq!(
            tags,
            [
                ("github.com", "Repo", Some("v1.2.3")),
                ("github.com", "other", Some("1.2.3")),
                ("github.com", "bin", Some("v1.2.3")),
                ("gitlab.com", "proj", Some("v1.2.3")),
                ("codeberg.org", "r", Some("v1.2.3")),
            ]
        );
        assert_eq!(f[3].forge, Forge::GitLab);
    }

    #[tokio::test]
    async fn owner_change_is_a_mismatch_without_network() {
        let c = ForgeClient::new(None);
        let old = si(&["https://github.com/real/app/archive/v1.0.tar.gz"]);
        let new = si(&["https://github.com/reaI/app/archive/v1.1.tar.gz"]);
        let r = c.corroborate(Some(&old), &new).await;
        assert_eq!(r.result, Result_::Mismatch, "{}", r.detail);
        let none = c.corroborate(Some(&old), &si(&["https://example.org/x.tar.gz"])).await;
        assert_eq!(none.result, Result_::Unconfirmed);
    }
}
