//! AUR access: the RPC for metadata and bare git mirrors for content. The
//! website itself sits behind bot protection, so nothing here scrapes HTML.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AurInfo {
    pub name: String,
    pub package_base: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "URL", default)]
    pub url: Option<String>,
    #[serde(rename = "URLPath", default)]
    pub url_path: Option<String>,
    pub last_modified: i64,
    pub first_submitted: i64,
    #[serde(default)]
    pub maintainer: Option<String>,
    #[serde(default)]
    pub submitter: Option<String>,
    #[serde(default)]
    pub co_maintainers: Vec<String>,
    #[serde(default)]
    pub out_of_date: Option<i64>,
    pub num_votes: i64,
    pub popularity: f64,
    #[serde(default)]
    pub depends: Vec<String>,
    #[serde(default)]
    pub make_depends: Vec<String>,
    #[serde(default)]
    pub check_depends: Vec<String>,
    #[serde(default)]
    pub opt_depends: Vec<String>,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub license: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Vec<AurInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

pub struct Aur {
    rpc_url: String,
    git_url: String,
    dir: PathBuf,
    http: reqwest::Client,
}

/// Names per RPC request; the AUR rejects very long query strings.
const RPC_CHUNK: usize = 100;

impl Aur {
    pub fn new(rpc_url: &str, git_url: &str, bulk_dir: &std::path::Path) -> Result<Self> {
        let dir = bulk_dir.join("aur");
        std::fs::create_dir_all(&dir)?;
        Ok(Aur {
            rpc_url: rpc_url.trim_end_matches('/').to_string(),
            git_url: git_url.trim_end_matches('/').to_string(),
            dir,
            http: reqwest::Client::builder()
                .user_agent(format!("repoman/{}", repoman_common::VERSION))
                .build()?,
        })
    }

    // ---- RPC --------------------------------------------------------------

    /// `multiinfo` for any number of package names, chunked. Names that do
    /// not exist are simply absent from the result.
    pub async fn info(&self, names: &[String]) -> Result<Vec<AurInfo>> {
        let mut out = Vec::new();
        for chunk in names.chunks(RPC_CHUNK) {
            let mut url = format!("{}/info?", self.rpc_url);
            for (i, n) in chunk.iter().enumerate() {
                if i > 0 {
                    url.push('&');
                }
                url.push_str("arg[]=");
                url.push_str(&urlencode(n));
            }
            let resp: RpcResponse = self
                .http
                .get(&url)
                .send()
                .await
                .context("AUR RPC request")?
                .error_for_status()
                .context("AUR RPC status")?
                .json()
                .await
                .context("AUR RPC body")?;
            if resp.kind == "error" {
                bail!("AUR RPC error: {}", resp.error.unwrap_or_default());
            }
            out.extend(resp.results);
        }
        Ok(out)
    }

    /// Packages whose `provides` (or name) match `name`.
    pub async fn search_provides(&self, name: &str) -> Result<Vec<AurInfo>> {
        let url = format!("{}/search/{}?by=provides", self.rpc_url, urlencode(name));
        let resp: RpcResponse = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if resp.kind == "error" {
            bail!("AUR RPC error: {}", resp.error.unwrap_or_default());
        }
        Ok(resp.results)
    }

    // ---- git mirrors --------------------------------------------------------

    pub fn mirror_path(&self, pkgbase: &str) -> PathBuf {
        self.dir.join(format!("{pkgbase}.git"))
    }

    async fn git(&self, args: &[&str], cwd: Option<&std::path::Path>) -> Result<String> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_TERMINAL_PROMPT", "0");
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        let out = cmd.output().await.context("spawning git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Clone the bare mirror if missing, otherwise fetch. Returns the head
    /// commit after the update.
    pub async fn sync(&self, pkgbase: &str) -> Result<String> {
        validate_pkgbase(pkgbase)?;
        let path = self.mirror_path(pkgbase);
        if path.join("HEAD").exists() {
            self.git(&["fetch", "--quiet", "--prune", "origin"], Some(&path))
                .await
                .with_context(|| format!("fetching {pkgbase}"))?;
        } else {
            let url = format!("{}/{pkgbase}.git", self.git_url);
            let tmp = self.dir.join(format!("{pkgbase}.git.part"));
            let _ = std::fs::remove_dir_all(&tmp);
            self.git(
                &["clone", "--quiet", "--mirror", &url, tmp.to_str().unwrap()],
                None,
            )
            .await
            .with_context(|| format!("cloning {pkgbase}"))?;
            std::fs::rename(&tmp, &path)?;
        }
        self.head(pkgbase).await
    }

    pub async fn head(&self, pkgbase: &str) -> Result<String> {
        let path = self.mirror_path(pkgbase);
        let out = self.git(&["rev-parse", "HEAD"], Some(&path)).await?;
        let h = out.trim().to_string();
        if h.len() != 40 {
            bail!("{pkgbase}: unexpected HEAD {h:?}");
        }
        Ok(h)
    }

    /// Is the mirror present and the commit known to it?
    pub async fn has_commit(&self, pkgbase: &str, commit: &str) -> bool {
        let path = self.mirror_path(pkgbase);
        self.git(&["cat-file", "-e", &format!("{commit}^{{commit}}")], Some(&path))
            .await
            .is_ok()
    }

    /// File contents at a commit, or None if the path does not exist there.
    pub async fn show(&self, pkgbase: &str, commit: &str, file: &str) -> Result<Option<String>> {
        let path = self.mirror_path(pkgbase);
        match self.git(&["show", &format!("{commit}:{file}")], Some(&path)).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.to_string().contains("does not exist") || e.to_string().contains("exists on disk, but not in") => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Paths tracked at a commit.
    pub async fn ls(&self, pkgbase: &str, commit: &str) -> Result<Vec<String>> {
        let path = self.mirror_path(pkgbase);
        let out = self
            .git(&["ls-tree", "-r", "--name-only", commit], Some(&path))
            .await?;
        Ok(out.lines().map(str::to_string).collect())
    }

    /// Uncompressed tar of the tree at a commit.
    pub async fn archive(&self, pkgbase: &str, commit: &str) -> Result<Vec<u8>> {
        let path = self.mirror_path(pkgbase);
        let out = Command::new("git")
            .args(["archive", "--format=tar", &format!("--prefix={pkgbase}/"), commit])
            .current_dir(&path)
            .stdin(Stdio::null())
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "git archive {pkgbase}@{commit}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Commits reachable from `to` but not from `from`, newest first.
    pub async fn commits_between(&self, pkgbase: &str, from: Option<&str>, to: &str) -> Result<Vec<CommitInfo>> {
        let path = self.mirror_path(pkgbase);
        let range = match from {
            Some(f) => format!("{f}..{to}"),
            None => to.to_string(),
        };
        let out = self
            .git(
                &["log", "--format=%H%x1f%an <%ae>%x1f%aI%x1f%s", &range],
                Some(&path),
            )
            .await?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let mut it = l.split('\x1f');
                Some(CommitInfo {
                    hash: it.next()?.to_string(),
                    author: it.next()?.to_string(),
                    date: it.next()?.to_string(),
                    subject: it.next().unwrap_or("").to_string(),
                })
            })
            .collect())
    }
}

fn validate_pkgbase(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('-')
        && !s.starts_with('.')
        && s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'_' | b'+' | b'-'));
    if !ok {
        bail!("invalid package base name {s:?}");
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rpc_payload() {
        let body = r#"{"resultcount":1,"results":[{"Depends":["pacman>6.1","git"],"Description":"d","FirstSubmitted":1475688004,"ID":2316242,"Keywords":["AUR"],"LastModified":1793196922,"License":["GPL-3.0-or-later"],"Maintainer":"jguer","MakeDepends":["go>=1.26"],"Name":"yay","NumVotes":2649,"OptDepends":["sudo"],"OutOfDate":null,"PackageBase":"yay","PackageBaseID":115973,"Popularity":14.03,"Submitter":"jguer","URL":"https://github.com/Jguer/yay","URLPath":"/cgit/aur.git/snapshot/yay.tar.gz","Version":"13.1.0-1"}],"type":"multiinfo","version":5}"#;
        let r: RpcResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.kind, "multiinfo");
        let p = &r.results[0];
        assert_eq!(p.package_base, "yay");
        assert_eq!(p.maintainer.as_deref(), Some("jguer"));
        assert_eq!(p.make_depends, ["go>=1.26"]);
        assert!(p.out_of_date.is_none());
        assert!(p.co_maintainers.is_empty());
    }

    #[test]
    fn rejects_bad_names() {
        assert!(validate_pkgbase("yay").is_ok());
        assert!(validate_pkgbase("lib32-foo+bar.baz_1").is_ok());
        assert!(validate_pkgbase("../etc").is_err());
        assert!(validate_pkgbase("-x").is_err());
        assert!(validate_pkgbase("a b").is_err());
    }

    #[test]
    fn urlencodes() {
        assert_eq!(urlencode("lib32-foo+bar"), "lib32-foo%2Bbar");
    }
}
