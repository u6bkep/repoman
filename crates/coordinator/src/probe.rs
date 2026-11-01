//! Upstream VCS probing from the coordinator: the revision a makepkg VCS
//! source would fetch, by `git ls-remote`, without cloning and without a
//! worker. Information gathering never leaves the coordinator; workers only
//! execute what the operator (or the gate) authorised.

use anyhow::{Context, Result, bail};
use repoman_alpm::vcs::VcsSource;

/// Resolve every VCS source of a package to (source entry, revision).
/// Non-VCS entries are skipped; the first unreachable upstream fails the
/// probe for the package.
pub async fn resolve(sources: &[String]) -> Result<Vec<(String, String)>> {
    let mut revisions = Vec::new();
    for s in sources {
        let Some(vcs) = VcsSource::parse(s) else { continue };
        let rev = match &vcs.fragment {
            Some((k, v)) if k == "commit" => v.clone(),
            Some((k, v)) if k == "tag" => ls_remote(&vcs.url, &format!("refs/tags/{v}")).await?,
            Some((k, v)) if k == "branch" => ls_remote(&vcs.url, &format!("refs/heads/{v}")).await?,
            _ => ls_remote(&vcs.url, "HEAD").await?,
        };
        revisions.push((s.clone(), rev));
    }
    Ok(revisions)
}

async fn ls_remote(url: &str, refname: &str) -> Result<String> {
    // An unreachable upstream must not stall a check; git's own connect
    // timeout can run to minutes.
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
        let (Some(hash), Some(name)) = (it.next(), it.next()) else { continue };
        if name.ends_with("^{}") {
            return Ok(hash.to_string());
        }
        if best.is_none() {
            best = Some(hash.to_string());
        }
    }
    best.with_context(|| format!("{url} has no ref {refname}"))
}
