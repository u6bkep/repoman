//! Feeds the deterministic gate (`repoman-gate`) from the coordinator's
//! state: mirrors, snapshots, baselines, stream indexes, and an inspect job
//! that regenerates the SRCINFO in a sandbox.

use std::collections::HashMap;

use anyhow::{Context, Result};
use repoman_alpm::depspec::DepSpec;
use repoman_alpm::srcinfo::Srcinfo;
use repoman_common::proto::{ImageRef, JobKind, JobResult};
use repoman_gate::{GateInput, Verdict, evaluate};
use repoman_store::{BaselineRow, SnapshotRow};

use crate::AppState;
use crate::engine::wait_job;

/// Maintainer plus co-maintainers, sorted, as stored on the baseline and
/// compared at review time.
pub fn maintainer_set(snap: Option<&SnapshotRow>) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if let Some(s) = snap {
        v.extend(s.maintainer.iter().cloned());
        v.extend(s.comaintainers.iter().cloned());
    }
    v.sort();
    v.dedup();
    v
}

/// Install scripts and hooks referenced by the SRCINFO or present in the
/// tree, with their contents at `commit`.
async fn scripts(app: &AppState, pkgbase: &str, commit: &str, si: &Srcinfo, files: &[String]) -> Vec<(String, String)> {
    let mut names: Vec<String> = si.base.values("install").into_iter().map(str::to_string).collect();
    for (_, sec) in &si.packages {
        for n in sec.values("install") {
            if !names.iter().any(|x| x == n) {
                names.push(n.to_string());
            }
        }
    }
    for f in files {
        if (f.ends_with(".hook") || f.ends_with(".install")) && !names.contains(f) {
            names.push(f.clone());
        }
    }
    let mut out = Vec::new();
    for n in names {
        if let Ok(Some(text)) = app.aur.show(pkgbase, commit, &n).await {
            out.push((n, text));
        }
    }
    out
}

/// For a package whose git repository is the project itself (a `source_url`
/// package such as repoman), the tree is mostly source code and every commit
/// adds files. Only the packaging files count for the gate's file-list rule:
/// PKGBUILD, .SRCINFO, install scripts, hooks, and local `source=` entries.
pub fn packaging_files(si: &Srcinfo, files: &[String]) -> Vec<String> {
    let mut local: Vec<String> = Vec::new();
    for (_, sec) in std::iter::once((&si.pkgbase, &si.base)).chain(si.packages.iter().map(|(n, s)| (n, s))) {
        for (k, v) in &sec.fields {
            if k == "source" || k.starts_with("source_") {
                let name = v.split_once("::").map(|(n, _)| n).unwrap_or(v);
                if !name.contains("://") && !name.starts_with("git+") {
                    local.push(name.to_string());
                }
            }
            if k == "install" || k == "changelog" {
                local.push(v.clone());
            }
        }
    }
    files
        .iter()
        .filter(|f| {
            f.as_str() == "PKGBUILD"
                || f.as_str() == ".SRCINFO"
                || f.ends_with(".install")
                || f.ends_with(".hook")
                || local.iter().any(|l| l == *f)
        })
        .cloned()
        .collect()
}

/// Regenerate the SRCINFO at `head` in a sandbox and compare it with the
/// committed one. `None` when no inspection could run.
async fn srcinfo_matches(app: &AppState, run_id: &str, pkgbase: &str, head: &str, committed: &Srcinfo, images: &HashMap<String, ImageRef>) -> Option<bool> {
    let image = app
        .config
        .streams_sorted()
        .into_iter()
        .find_map(|s| images.get(&s.name).cloned())?;
    let job = app
        .store
        .create_job_for(
            &JobKind::Inspect {
                pkgbase: pkgbase.to_string(),
                aur_commit: head.to_string(),
                image,
                source_url: format!("{}/api/sources/{pkgbase}/{head}.tar", app.config.public_url),
            },
            Some(pkgbase),
            None,
            None,
            Some(run_id),
        )
        .ok()?;
    app.hub.notify();
    let row = match wait_job(app, &job.id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(pkgbase, error = %e, "inspect job failed");
            return None;
        }
    };
    let Some(JobResult::Inspect { srcinfo }) = row.result else {
        tracing::warn!(pkgbase, error = ?row.error, "inspect job produced no SRCINFO");
        return None;
    };
    match Srcinfo::parse(&srcinfo) {
        Ok(regenerated) => Some(regenerated.diff(committed).is_empty() && committed.diff(&regenerated).is_empty()),
        Err(e) => {
            tracing::warn!(pkgbase, error = %e, "regenerated SRCINFO does not parse");
            Some(false)
        }
    }
}

/// What the gate read, kept for the LLM reviewer so both see the same
/// material.
pub struct Assessment {
    pub verdict: Verdict,
    /// Forge corroboration of the sources, when checked.
    pub corroboration: Option<crate::forge::Corroboration>,
    pub old_pkgbuild: Option<String>,
    pub new_pkgbuild: String,
    pub old_scripts: Vec<(String, String)>,
    pub new_scripts: Vec<(String, String)>,
    pub maintainers_before: Vec<String>,
    pub maintainers_now: Vec<String>,
}

/// Assess the change from `baseline` (or, without one, lint the head alone).
/// `head_si` is the committed SRCINFO at `head`.
pub async fn assess(
    app: &AppState,
    run_id: &str,
    pkgbase: &str,
    baseline: Option<&BaselineRow>,
    head: &str,
    head_si: &Srcinfo,
    images: &HashMap<String, ImageRef>,
    self_hosted: bool,
    history_rewritten: bool,
) -> Result<Assessment> {
    let old_si = match baseline {
        Some(b) => {
            let text = app
                .aur
                .show(pkgbase, &b.aur_commit, ".SRCINFO")
                .await?
                .with_context(|| format!("{pkgbase}: no .SRCINFO at baseline {}", b.aur_commit))?;
            Srcinfo::parse(&text)?
        }
        None => head_si.clone(),
    };
    let new_pkgbuild = app.aur.show(pkgbase, head, "PKGBUILD").await?.unwrap_or_default();
    let old_pkgbuild = match baseline {
        Some(b) => app.aur.show(pkgbase, &b.aur_commit, "PKGBUILD").await?,
        None => None,
    };
    let mut new_files = app.aur.ls(pkgbase, head).await.unwrap_or_default();
    let mut old_files = match baseline {
        Some(b) => app.aur.ls(pkgbase, &b.aur_commit).await.unwrap_or_default(),
        None => Vec::new(),
    };
    if self_hosted {
        new_files = packaging_files(head_si, &new_files);
        old_files = packaging_files(&old_si, &old_files);
    }
    let new_scripts = scripts(app, pkgbase, head, head_si, &new_files).await;
    let old_scripts = match baseline {
        Some(b) => scripts(app, pkgbase, &b.aur_commit, &old_si, &old_files).await,
        None => Vec::new(),
    };
    let snap = app.store.get_snapshot(pkgbase)?;
    let maintainer_changed = match baseline {
        Some(b) if !b.maintainers.is_empty() => maintainer_set(snap.as_ref()) != b.maintainers,
        _ => false,
    };
    // The poll records an adoption as a signal that expires with the
    // adoption quarantine; while it stands, every change is a new
    // maintainer's change.
    let adopted_from_orphan = app.store.active_signal(pkgbase, "aur", "adopted")?.is_some();
    let srcinfo_mismatch = match baseline {
        Some(_) => srcinfo_matches(app, run_id, pkgbase, head, head_si, images).await.map(|ok| !ok),
        None => None,
    };
    let streams = &app.streams;
    let names = streams.names();
    let is_official = |name: &str| {
        let dep = DepSpec::unversioned(name);
        names.iter().any(|n| streams.get(n).is_some_and(|ix| ix.index.satisfies(&dep)))
    };
    let mut verdict = evaluate(&GateInput {
        old: &old_si,
        new: head_si,
        old_pkgbuild: old_pkgbuild.as_deref(),
        new_pkgbuild: &new_pkgbuild,
        new_scripts: &new_scripts,
        old_scripts: &old_scripts,
        old_files: &old_files,
        new_files: &new_files,
        maintainer_changed,
        adopted_from_orphan,
        history_rewritten,
        srcinfo_mismatch,
        is_official: &is_official,
    });
    // Corroborate routine-looking updates against the upstream forge. A
    // structural change is going to a human anyway; this is what catches a
    // "routine" bump whose version was never released upstream, or whose
    // sources quietly moved to another owner.
    let mut corroboration = None;
    if baseline.is_some() && app.config.forge_checks && verdict.diff_class != repoman_gate::DiffClass::Structural {
        let c = app.forge.corroborate(Some(&old_si), head_si).await;
        if c.result == crate::forge::Result_::Mismatch {
            verdict.reasons.push(repoman_gate::Reason {
                kind: "upstream_not_corroborated".into(),
                detail: c.detail.clone(),
                class: verdict.diff_class,
                risk: repoman_gate::Risk::Medium,
            });
            verdict.floor_risk = verdict.floor_risk.max(repoman_gate::Risk::Medium);
        }
        corroboration = Some(c);
    }
    Ok(Assessment {
        verdict,
        corroboration,
        old_pkgbuild,
        new_pkgbuild,
        old_scripts,
        new_scripts,
        maintainers_before: baseline.map(|b| b.maintainers.clone()).unwrap_or_default(),
        maintainers_now: maintainer_set(snap.as_ref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaging_files_keep_only_what_makepkg_reads() {
        let si = Srcinfo::parse(
            "pkgbase = x\n\tpkgver = 1\n\tpkgrel = 1\n\tinstall = x.install\n\tsource = x::git+https://h/x.git\n\tsource = fix.patch\n\tsource_x86_64 = https://h/x.bin\npkgname = x\n",
        )
        .unwrap();
        let files: Vec<String> = ["PKGBUILD", ".SRCINFO", "x.install", "fix.patch", "src/main.rs", "Cargo.toml", "pacman.hook"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(packaging_files(&si, &files), ["PKGBUILD", ".SRCINFO", "x.install", "fix.patch", "pacman.hook"]);
    }
}
