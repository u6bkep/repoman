//! Upstream VCS probing: resolve the revision a makepkg VCS source would
//! fetch, without cloning.

use anyhow::{Context, Result, bail};
use repoman_common::proto::JobResult;

use crate::exec::JobCtx;

/// A parsed makepkg VCS source: `[name::]git+URL[#fragment]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VcsSource {
    pub url: String,
    /// `branch=x`, `tag=x`, or `commit=x`.
    pub fragment: Option<(String, String)>,
}

impl VcsSource {
    pub fn parse(source: &str) -> Option<VcsSource> {
        let spec = match source.split_once("::") {
            Some((_, rest)) => rest,
            None => source,
        };
        let spec = spec.strip_prefix("git+").or_else(|| {
            if spec.starts_with("git://") {
                Some(spec)
            } else {
                None
            }
        })?;
        let (url, frag) = match spec.split_once('#') {
            Some((u, f)) => (u, Some(f)),
            None => (spec, None),
        };
        // Strip a `?signed` query.
        let url = url.split('?').next().unwrap_or(url).to_string();
        let fragment = frag.and_then(|f| f.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string()));
        Some(VcsSource { url, fragment })
    }

    pub fn is_pinned(&self) -> bool {
        matches!(&self.fragment, Some((k, _)) if k == "commit" || k == "tag")
    }
}

pub async fn run_probe(ctx: &JobCtx, sources: &[String]) -> Result<JobResult> {
    let mut revisions = Vec::new();
    for s in sources {
        let Some(vcs) = VcsSource::parse(s) else {
            continue;
        };
        let rev = match &vcs.fragment {
            Some((k, v)) if k == "commit" => v.clone(),
            Some((k, v)) if k == "tag" => ls_remote(&vcs.url, &format!("refs/tags/{v}")).await?,
            Some((k, v)) if k == "branch" => ls_remote(&vcs.url, &format!("refs/heads/{v}")).await?,
            _ => ls_remote(&vcs.url, "HEAD").await?,
        };
        ctx.log(repoman_common::proto::Fd::System, format!("{} -> {rev}", vcs.url));
        revisions.push((s.clone(), rev));
    }
    Ok(JobResult::Probe { revisions })
}

async fn ls_remote(url: &str, refname: &str) -> Result<String> {
    // An unreachable upstream must not stall the whole run; git's own
    // connect timeout can run to minutes.
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["ls-remote", "--", url, refname])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(45), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("git ls-remote {url}: no answer within 45 s"))?
        .context("spawning git ls-remote")?;
    if !out.status.success() {
        bail!("git ls-remote {url} {refname}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Prefer the peeled tag object if present.
    let mut best: Option<String> = None;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(hash), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        if name.ends_with("^{}") {
            return Ok(hash.to_string());
        }
        if best.is_none() {
            best = Some(hash.to_string());
        }
    }
    best.with_context(|| format!("{url} has no ref {refname}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vcs_sources() {
        let s = VcsSource::parse("yay::git+https://github.com/Jguer/yay.git#branch=main").unwrap();
        assert_eq!(s.url, "https://github.com/Jguer/yay.git");
        assert_eq!(s.fragment, Some(("branch".into(), "main".into())));
        assert!(!s.is_pinned());
        let t = VcsSource::parse("git+https://x/y.git?signed#tag=v1.2").unwrap();
        assert_eq!(t.url, "https://x/y.git");
        assert!(t.is_pinned());
        assert!(VcsSource::parse("https://x/y.tar.gz").is_none());
        assert!(VcsSource::parse("foo.patch").is_none());
    }
}
