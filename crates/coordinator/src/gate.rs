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

/// Regenerate the SRCINFO at `head` in a sandbox and compare it with the
/// committed one. `None` when no inspection could run.
async fn srcinfo_matches(app: &AppState, pkgbase: &str, head: &str, committed: &Srcinfo, images: &HashMap<String, ImageRef>) -> Option<bool> {
    let image = app
        .config
        .streams_sorted()
        .into_iter()
        .find_map(|s| images.get(&s.name).cloned())?;
    let job = app
        .store
        .create_job(
            &JobKind::Inspect {
                pkgbase: pkgbase.to_string(),
                aur_commit: head.to_string(),
                image,
                source_url: format!("{}/api/sources/{pkgbase}/{head}.tar", app.config.public_url),
            },
            Some(pkgbase),
            None,
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

/// Assess the change from `baseline` (or, without one, lint the head alone).
/// `head_si` is the committed SRCINFO at `head`.
pub async fn assess(
    app: &AppState,
    pkgbase: &str,
    baseline: Option<&BaselineRow>,
    head: &str,
    head_si: &Srcinfo,
    images: &HashMap<String, ImageRef>,
) -> Result<Verdict> {
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
    let new_files = app.aur.ls(pkgbase, head).await.unwrap_or_default();
    let old_files = match baseline {
        Some(b) => app.aur.ls(pkgbase, &b.aur_commit).await.unwrap_or_default(),
        None => Vec::new(),
    };
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
    let srcinfo_mismatch = match baseline {
        Some(_) => srcinfo_matches(app, pkgbase, head, head_si, images).await.map(|ok| !ok),
        None => None,
    };
    let streams = &app.streams;
    let names = streams.names();
    let is_official = |name: &str| {
        let dep = DepSpec::unversioned(name);
        names.iter().any(|n| streams.get(n).is_some_and(|ix| ix.index.satisfies(&dep)))
    };
    Ok(evaluate(&GateInput {
        old: &old_si,
        new: head_si,
        old_pkgbuild: old_pkgbuild.as_deref(),
        new_pkgbuild: &new_pkgbuild,
        new_scripts: &new_scripts,
        old_scripts: &old_scripts,
        old_files: &old_files,
        new_files: &new_files,
        maintainer_changed,
        srcinfo_mismatch,
        is_official: &is_official,
    }))
}
