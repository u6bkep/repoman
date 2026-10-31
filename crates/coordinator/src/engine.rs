//! The update run. One run is active at a time. Each step persists its
//! progress so the UI can show where a run is, and a run that needs the
//! operator parks in `needs_input` until `start_or_resume` is called again.
//!
//! M1 gate: every `update` and `onboarding` candidate waits for a manual
//! decision; `vcs` and `dependency_rebuild` candidates are auto-approved.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use repoman_alpm::pacman_conf::Repo;
use repoman_alpm::srcinfo::Srcinfo;
use repoman_common::clock;
use repoman_common::config::StreamConfig;
use repoman_common::proto::{ImageRef, JobKind, JobResult, JobState};
use repoman_store::{CandidateRow, NewCandidate, PackageRow, SnapshotRow};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::planner::{self, Input};
use crate::publisher::Publisher;

pub struct Engine {
    running: AtomicBool,
}

impl Engine {
    pub fn new() -> Arc<Self> {
        Arc::new(Engine {
            running: AtomicBool::new(false),
        })
    }

    /// Start a run, or resume the active one. Returns the run id. The run
    /// executes in a background task.
    pub fn start_or_resume(self: &Arc<Self>, app: AppState) -> Result<String> {
        if self.running.swap(true, Ordering::SeqCst) {
            if let Some(r) = app.store.active_run()? {
                return Ok(r.id);
            }
            self.running.store(false, Ordering::SeqCst);
            bail!("engine busy without an active run; retry");
        }
        let run = match app.store.active_run()? {
            Some(r) => r,
            None => app.store.create_run()?,
        };
        let id = run.id.clone();
        let engine = self.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            let id = task_id;
            let result = execute(&app, &id).await;
            match result {
                Ok(RunEnd::Done(summary)) => {
                    let _ = app.store.finish_run(&id, "done", &summary);
                    tracing::info!(run = %id, "run finished");
                }
                Ok(RunEnd::NeedsInput(n)) => {
                    let _ = app.store.set_run_step(&id, "needs_input", Some("gate"));
                    tracing::info!(run = %id, pending = n, "run waiting for operator decisions");
                }
                Err(e) => {
                    tracing::error!(run = %id, error = %format!("{e:#}"), "run aborted");
                    let _ = app.store.finish_run(&id, "aborted", &serde_json::json!({"error": format!("{e:#}")}));
                }
            }
            engine.running.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

enum RunEnd {
    Done(serde_json::Value),
    NeedsInput(usize),
}

async fn step(app: &AppState, run_id: &str, name: &str) -> Result<()> {
    tracing::info!(run = %run_id, step = name, "step");
    app.store.set_run_step(run_id, "running", Some(name))?;
    app.store.event("run.step", Some(run_id), serde_json::json!({"step": name}))?;
    Ok(())
}

async fn execute(app: &AppState, run_id: &str) -> Result<RunEnd> {
    // A previous attempt at this run may have died mid-build. Its jobs are
    // stale: the plan below creates new ones.
    for c in app.store.candidates_in_states(&["building", "built"])? {
        app.store.set_candidate_state(&c.id, "approved")?;
    }
    let cancelled = app.store.cancel_open_jobs(&["build", "image", "inspect", "probe"])?;
    if !cancelled.is_empty() {
        tracing::warn!(count = cancelled.len(), "cancelled stale jobs from an interrupted run");
        app.hub.cancel_jobs(&cancelled);
    }

    let mut summary = serde_json::json!({});
    // Discovery can add packages; loop until a pass adds nothing.
    for pass in 0..8 {
        step(app, run_id, "streams").await?;
        app.streams.refresh_all().await.context("refreshing streams")?;
        // Repair: a stream with publications but no database file (first run
        // after a rename, or manual deletion) gets its database rebuilt.
        for s in app.config.streams_sorted() {
            if !app.publisher.has_db(&s.name) {
                let live: Vec<(String, String)> = app
                    .store
                    .live_artifacts(&s.name)?
                    .into_iter()
                    .map(|a| (a.sha256, a.filename))
                    .collect();
                if !live.is_empty() {
                    tracing::warn!(stream = %s.name, "repo database missing; rebuilding from publications");
                    app.publisher.rebuild(&app.blobs, &s.name, &live).await?;
                }
            }
        }

        step(app, run_id, "images").await?;
        let mut images: HashMap<String, ImageRef> = HashMap::new();
        for s in app.config.streams_sorted() {
            images.insert(s.name.clone(), ensure_image(app, s).await?);
        }

        step(app, run_id, "poll").await?;
        poll_aur(app).await?;

        step(app, run_id, "candidates").await?;
        create_candidates(app, run_id, &images).await?;

        let pending = app.store.candidates_in_states(&["pending"])?;
        if !pending.is_empty() {
            return Ok(RunEnd::NeedsInput(pending.len()));
        }

        step(app, run_id, "plan").await?;
        let planned = plan_all(app, run_id).await?;
        if planned.discovered > 0 {
            tracing::info!(discovered = planned.discovered, pass, "new AUR dependencies discovered; looping");
            continue;
        }

        step(app, run_id, "build").await?;
        let built = build_all(app, run_id, &planned, &images).await?;

        // Candidates whose builds all succeeded were published as they
        // completed; this pass handles partial failures and baseline-only
        // rebuilds that carry no candidate.
        step(app, run_id, "publish").await?;
        let scope = PublishScope {
            only: None,
            exclude: built.published_candidates.clone(),
        };
        let published = built.published_count + publish_all(app, run_id, &planned, &built.results, &scope).await?;

        summary = serde_json::json!({
            "streams": planned.per_stream.keys().collect::<Vec<_>>(),
            "builds_planned": planned.per_stream.values().map(|v| v.len()).sum::<usize>(),
            "builds_succeeded": built.results.values().filter(|r| r.is_ok()).count(),
            "builds_failed": built.results.values().filter(|r| r.is_err()).count(),
            "published": published,
            "skipped": planned.skipped,
        });
        break;
    }

    step(app, run_id, "cleanup").await?;
    let _ = app.publisher.remove_staging(run_id);
    Ok(RunEnd::Done(summary))
}

// ---- images -------------------------------------------------------------

async fn ensure_image(app: &AppState, stream: &StreamConfig) -> Result<ImageRef> {
    let idx = app
        .streams
        .get(&stream.name)
        .context("stream index missing after refresh")?;
    let fingerprint = idx.fingerprint();
    if let Some(img) = app.store.latest_image(&stream.name)? {
        let age = clock::seconds_between(&img.created_at, &clock::now()).unwrap_or(f64::MAX);
        let fresh = age < app.config.image_max_age_secs as f64 || img.stream_fingerprint.as_deref() == Some(&fingerprint);
        if fresh && app.blobs.exists(&img.sha256) {
            return Ok(image_ref(app, &img.sha256, &img.filename));
        }
    }
    tracing::info!(stream = %stream.name, "building base image");
    let job = app.store.create_job(
        &JobKind::Image {
            stream: stream.name.clone(),
            pacman_conf: app.streams.image_pacman_conf(stream),
            packages: app.config.image_packages.clone(),
            keyrings: stream.keyrings.clone(),
            makepkg_conf_extra: app.config.makepkg_conf_extra.clone(),
            upload_url: format!("{}/api/blobs", app.config.public_url),
        },
        None,
        Some(&stream.name),
    )?;
    app.hub.notify();
    let row = wait_job(app, &job.id).await?;
    match row.result {
        Some(JobResult::Image {
            sha256,
            size,
            filename,
            packages,
        }) if row.state == JobState::Done => {
            app.store
                .insert_image(&sha256, &stream.name, &filename, size, Some(&fingerprint), &packages)?;
            for old in app.store.retire_old_images(&stream.name, 2)? {
                tracing::info!(image = %old, "retired old image");
                let _ = app.blobs.delete(&old);
            }
            Ok(image_ref(app, &sha256, &filename))
        }
        _ => bail!(
            "image build for {} failed: {}",
            stream.name,
            row.error.unwrap_or_else(|| "no result".into())
        ),
    }
}

fn image_ref(app: &AppState, sha256: &str, filename: &str) -> ImageRef {
    ImageRef {
        sha256: sha256.to_string(),
        url: format!("{}/store/{sha256}/{filename}", app.config.public_url),
    }
}

/// Wait for a job to reach a terminal state.
pub async fn wait_job(app: &AppState, job_id: &str) -> Result<repoman_store::JobRow> {
    let mut rx = app.hub.subscribe();
    loop {
        if let Some(row) = app.store.get_job(job_id)? {
            if row.state.is_terminal() {
                return Ok(row);
            }
        }
        match tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await {
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) | Err(_) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => bail!("hub closed"),
        }
    }
}

// ---- poll -------------------------------------------------------------------

async fn poll_aur(app: &AppState) -> Result<()> {
    let packages = app.store.list_packages()?;
    if packages.is_empty() {
        return Ok(());
    }
    // Packages from other git repositories: no RPC, the commit is the clock.
    for p in packages.iter().filter(|p| p.source_url.is_some()) {
        let url = p.source_url.as_deref().unwrap();
        let head = match app.aur.sync_from(&p.pkgbase, url).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(pkgbase = %p.pkgbase, %url, error = %e, "syncing git source");
                continue;
            }
        };
        let last_modified = app.aur.commit_time(&p.pkgbase, &head).await.unwrap_or(0);
        let version = srcinfo_at(app, &p.pkgbase, &head).await.ok().map(|s| s.version());
        let prev = app.store.get_snapshot(&p.pkgbase)?;
        app.store.upsert_snapshot(&SnapshotRow {
            pkgbase: p.pkgbase.clone(),
            fetched_at: clock::now(),
            version,
            last_modified,
            maintainer: prev.as_ref().and_then(|s| s.maintainer.clone()),
            submitter: None,
            comaintainers: vec![],
            out_of_date: None,
            votes: None,
            popularity: None,
            head_commit: Some(head),
        })?;
    }
    let packages: Vec<PackageRow> = packages.into_iter().filter(|p| p.source_url.is_none()).collect();
    if packages.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = packages.iter().map(|p| p.pkgbase.clone()).collect();
    let infos = app.aur.info(&names).await.context("AUR RPC")?;
    // Only pkgbase rows matter; the RPC returns one entry per pkgname.
    let mut by_base: HashMap<String, &crate::aur::AurInfo> = HashMap::new();
    for i in &infos {
        by_base.entry(i.package_base.clone()).or_insert(i);
    }
    for p in &packages {
        let Some(info) = by_base.get(&p.pkgbase) else {
            tracing::warn!(pkgbase = %p.pkgbase, "not found on the AUR");
            app.store.event("aur.missing", Some(&p.pkgbase), serde_json::json!({}))?;
            continue;
        };
        let prev = app.store.get_snapshot(&p.pkgbase)?;
        let changed = prev.as_ref().map(|s| s.last_modified != info.last_modified).unwrap_or(true);
        let mut snap = SnapshotRow {
            pkgbase: p.pkgbase.clone(),
            fetched_at: clock::now(),
            version: Some(info.version.clone()),
            last_modified: info.last_modified,
            maintainer: info.maintainer.clone(),
            submitter: info.submitter.clone(),
            comaintainers: info.co_maintainers.clone(),
            out_of_date: info.out_of_date,
            votes: Some(info.num_votes),
            popularity: Some(info.popularity),
            head_commit: None,
        };
        let need_sync = changed || !app.aur.mirror_path(&p.pkgbase).join("HEAD").exists();
        if need_sync {
            match app.aur.sync(&p.pkgbase).await {
                Ok(head) => snap.head_commit = Some(head),
                Err(e) => tracing::warn!(pkgbase = %p.pkgbase, error = %e, "syncing AUR mirror"),
            }
        }
        if let Some(prev) = &prev {
            if prev.maintainer != info.maintainer {
                app.store.event(
                    "aur.maintainer_changed",
                    Some(&p.pkgbase),
                    serde_json::json!({"from": prev.maintainer, "to": info.maintainer}),
                )?;
            }
        }
        app.store.upsert_snapshot(&snap)?;
    }
    Ok(())
}

// ---- candidates ---------------------------------------------------------------

async fn srcinfo_at(app: &AppState, pkgbase: &str, commit: &str) -> Result<Srcinfo> {
    let text = app
        .aur
        .show(pkgbase, commit, ".SRCINFO")
        .await?
        .with_context(|| format!("{pkgbase}@{commit} has no .SRCINFO"))?;
    Srcinfo::parse(&text).with_context(|| format!("parsing .SRCINFO of {pkgbase}@{commit}"))
}

fn srcinfo_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

async fn create_candidates(app: &AppState, run_id: &str, images: &HashMap<String, ImageRef>) -> Result<()> {
    let packages = app.store.list_packages()?;
    let mut probes: Vec<(PackageRow, String)> = Vec::new();
    for p in &packages {
        let Some(snap) = app.store.get_snapshot(&p.pkgbase)? else {
            continue;
        };
        let Some(head) = snap.head_commit.clone() else {
            continue;
        };
        let baseline = app.store.get_baseline(&p.pkgbase)?;
        let open = app.store.open_candidates(&p.pkgbase)?;
        // Carry open candidates into this run.
        for c in &open {
            if c.run_id != run_id {
                app.store.adopt_candidate(&c.id, run_id)?;
            }
        }
        match &baseline {
            None => {
                if !open.iter().any(|c| c.kind == "onboarding" && c.aur_commit.as_deref() == Some(&head)) {
                    app.store.supersede_candidates(&p.pkgbase, &head)?;
                    app.store.create_candidate(
                        run_id,
                        &NewCandidate {
                            pkgbase: &p.pkgbase,
                            kind: "onboarding",
                            aur_commit: Some(&head),
                            first_seen_last_modified: Some(snap.last_modified),
                            class: p.provenance_class.as_deref(),
                            state: "pending",
                            review: serde_json::json!({"note": "no baseline; first review"}),
                            ..Default::default()
                        },
                    )?;
                }
            }
            Some(b) if b.aur_commit != head => {
                if app.store.rejected_candidate_for(&p.pkgbase, &head)?.is_some() {
                    continue;
                }
                if !open.iter().any(|c| c.kind == "update" && c.aur_commit.as_deref() == Some(&head)) {
                    app.store.supersede_candidates(&p.pkgbase, &head)?;
                    let review = match (srcinfo_at(app, &p.pkgbase, &b.aur_commit).await, srcinfo_at(app, &p.pkgbase, &head).await) {
                        (Ok(old), Ok(new)) => {
                            let changes = old.diff(&new);
                            serde_json::json!({
                                "from": b.aur_commit, "to": head,
                                "version_from": old.version(), "version_to": new.version(),
                                "changes": changes,
                            })
                        }
                        (o, n) => serde_json::json!({
                            "from": b.aur_commit, "to": head,
                            "error": format!("{:?} / {:?}", o.err().map(|e| e.to_string()), n.err().map(|e| e.to_string())),
                        }),
                    };
                    let commits = app
                        .aur
                        .commits_between(&p.pkgbase, Some(&b.aur_commit), &head)
                        .await
                        .unwrap_or_default();
                    let mut review = review;
                    review["commits"] = serde_json::to_value(&commits)?;
                    app.store.create_candidate(
                        run_id,
                        &NewCandidate {
                            pkgbase: &p.pkgbase,
                            kind: "update",
                            aur_commit: Some(&head),
                            first_seen_last_modified: Some(snap.last_modified),
                            class: None,
                            state: "pending",
                            review,
                            ..Default::default()
                        },
                    )?;
                }
            }
            Some(_) => {
                // Baseline is current. VCS packages still need an upstream check.
                if p.kind == "vcs" && !open.iter().any(|c| c.kind == "vcs") {
                    probes.push((p.clone(), head));
                }
            }
        }
    }

    // Probe VCS upstreams in parallel through workers.
    let mut probe_jobs = Vec::new();
    for (p, head) in &probes {
        let si = match srcinfo_at(app, &p.pkgbase, head).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(pkgbase = %p.pkgbase, error = %e, "cannot read SRCINFO for probe");
                continue;
            }
        };
        let sources: Vec<String> = si.base.values("source").into_iter().map(str::to_string).collect();
        let job = app.store.create_job(
            &JobKind::Probe {
                pkgbase: p.pkgbase.clone(),
                sources,
            },
            Some(&p.pkgbase),
            None,
        )?;
        probe_jobs.push((p.clone(), head.clone(), job.id));
    }
    if !probe_jobs.is_empty() {
        app.hub.notify();
    }
    let _ = images;
    for (p, head, job_id) in probe_jobs {
        let row = wait_job(app, &job_id).await?;
        let Some(JobResult::Probe { revisions }) = row.result else {
            tracing::warn!(pkgbase = %p.pkgbase, "probe failed: {:?}", row.error);
            continue;
        };
        let rev = revisions.iter().map(|(_, r)| r.as_str()).collect::<Vec<_>>().join("+");
        if rev.is_empty() {
            continue;
        }
        // Compare with what is published on any member stream.
        let mut stale = false;
        for m in app.store.memberships(&p.pkgbase)? {
            if !(m.wanted || m.required) {
                continue;
            }
            let si = srcinfo_at(app, &p.pkgbase, &head).await?;
            let published = si
                .pkgnames()
                .first()
                .and_then(|n| app.store.live_artifact(&m.stream, n).ok().flatten());
            match published {
                Some(a) if a.upstream_rev.as_deref() == Some(&rev) => {}
                _ => stale = true,
            }
        }
        if stale {
            app.store.create_candidate(
                run_id,
                &NewCandidate {
                    pkgbase: &p.pkgbase,
                    kind: "vcs",
                    aur_commit: Some(&head),
                    upstream_rev: Some(&rev),
                    state: "approved",
                    review: serde_json::json!({"upstream_rev": rev}),
                    ..Default::default()
                },
            )?;
        }
    }
    Ok(())
}

// ---- plan -----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BuildItem {
    pub pkgbase: String,
    pub stream: String,
    pub aur_commit: String,
    pub candidate_id: Option<String>,
    pub install: Vec<String>,
    pub member_deps: Vec<String>,
    pub pkgnames: Vec<String>,
    pub validpgpkeys: Vec<String>,
    pub skip_pgp: bool,
    pub allow_network: bool,
    /// Repos this build is published to: its own stream, or every member
    /// stream for an arch=any build-once package.
    pub publish_streams: Vec<String>,
}

#[derive(Default)]
pub struct Planned {
    pub per_stream: BTreeMap<String, Vec<BuildItem>>,
    pub discovered: usize,
    pub skipped: Vec<serde_json::Value>,
}

/// Runtime interpreters whose install paths encode a version; an arch=any
/// package depending on one is not stream independent.
const VERSIONED_INTERPRETERS: &[&str] = &["python", "python2", "perl", "ruby", "lua", "lua51", "lua53", "lua54", "php", "ghc", "nodejs"];

/// Build once and publish to every member stream?
fn build_once(si: &Srcinfo, policy: &serde_json::Value, arch: &str) -> bool {
    if let Some(v) = policy.get("build_once").and_then(|v| v.as_bool()) {
        return v;
    }
    if si.architectures() != ["any"] {
        return false;
    }
    !si.pkgnames()
        .iter()
        .flat_map(|n| si.runtime_deps(n, arch))
        .any(|d| VERSIONED_INTERPRETERS.contains(&d.name.as_str()))
}

/// Member streams of a package in sort order.
fn member_streams(app: &AppState, pkgbase: &str) -> Result<Vec<String>> {
    let mut v: Vec<(i32, String)> = app
        .store
        .memberships(pkgbase)?
        .into_iter()
        .filter(|m| m.wanted || m.required)
        .filter_map(|m| app.config.stream(&m.stream).map(|s| (s.sort_order, s.name.clone())))
        .collect();
    v.sort();
    Ok(v.into_iter().map(|(_, n)| n).collect())
}

fn target_commit(cands: &[CandidateRow], baseline: Option<&str>) -> Option<(String, Option<String>)> {
    for c in cands {
        // Failed candidates stay approved in spirit: they are re-planned until
        // they succeed or a newer head supersedes them.
        if c.state == "approved" || c.state == "failed" {
            if let Some(commit) = &c.aur_commit {
                return Some((commit.clone(), Some(c.id.clone())));
            }
        }
    }
    baseline.map(|b| (b.to_string(), None))
}

async fn plan_all(app: &AppState, run_id: &str) -> Result<Planned> {
    let mut planned = Planned::default();
    let packages = app.store.list_packages()?;
    let mut srcinfos: HashMap<String, (String, Srcinfo)> = HashMap::new(); // pkgbase -> (commit, srcinfo)
    let mut candidate_of: HashMap<String, Option<String>> = HashMap::new();
    for p in &packages {
        let cands = app.store.open_candidates(&p.pkgbase)?;
        let baseline = app.store.get_baseline(&p.pkgbase)?;
        let Some((commit, cand)) = target_commit(&cands, baseline.as_ref().map(|b| b.aur_commit.as_str())) else {
            continue; // nothing approved and no baseline: still onboarding
        };
        match srcinfo_at(app, &p.pkgbase, &commit).await {
            Ok(si) => {
                srcinfos.insert(p.pkgbase.clone(), (commit, si));
                candidate_of.insert(p.pkgbase.clone(), cand);
            }
            Err(e) => {
                planned.skipped.push(serde_json::json!({"pkgbase": p.pkgbase, "reason": format!("{e:#}")}));
            }
        }
    }
    let mut known_aur: HashMap<String, String> = HashMap::new();
    for (pb, (_, si)) in &srcinfos {
        for n in si.provided_names(&app.config.arch) {
            known_aur.entry(n).or_insert_with(|| pb.clone());
        }
    }

    for stream in app.config.streams_sorted() {
        let idx = app.streams.get(&stream.name).context("stream index missing")?;
        let members = app.store.members_of_stream(&stream.name)?;
        let member_names: BTreeSet<String> = members.iter().map(|m| m.pkgbase.clone()).collect();

        // arch=any packages without versioned-interpreter dependencies build
        // once, on their lowest-sort member stream, and publish everywhere.
        let mut once_targets: HashMap<String, Vec<String>> = HashMap::new();
        let mut skip_here: HashSet<String> = HashSet::new();
        for m in &members {
            let Some((_, si)) = srcinfos.get(&m.pkgbase) else { continue };
            let policy = packages
                .iter()
                .find(|p| p.pkgbase == m.pkgbase)
                .map(|p| p.policy.clone())
                .unwrap_or(serde_json::Value::Null);
            if build_once(si, &policy, &app.config.arch) {
                let targets = member_streams(app, &m.pkgbase)?;
                if targets.first() == Some(&stream.name) {
                    once_targets.insert(m.pkgbase.clone(), targets);
                } else {
                    skip_here.insert(m.pkgbase.clone());
                }
            }
        }

        // Which members need a build on this stream?
        let mut needs: BTreeSet<String> = BTreeSet::new();
        for m in &members {
            if skip_here.contains(&m.pkgbase) {
                continue;
            }
            let Some((commit, si)) = srcinfos.get(&m.pkgbase) else { continue };
            let approved = candidate_of.get(&m.pkgbase).cloned().flatten().is_some();
            let targets = once_targets
                .get(&m.pkgbase)
                .cloned()
                .unwrap_or_else(|| vec![stream.name.clone()]);
            let published = targets.iter().all(|t| {
                si.pkgnames().iter().all(|n| {
                    app.store
                        .live_artifact(t, n)
                        .ok()
                        .flatten()
                        .is_some_and(|a| a.aur_commit.as_deref() == Some(commit.as_str()))
                })
            });
            if approved || !published {
                needs.insert(m.pkgbase.clone());
            }
        }

        let policies: HashMap<String, HashMap<String, String>> = packages
            .iter()
            .map(|p| {
                let choices = p
                    .policy
                    .get("provider_choices")
                    .and_then(|v| serde_json::from_value::<HashMap<String, String>>(v.clone()).ok())
                    .unwrap_or_default();
                (p.pkgbase.clone(), choices)
            })
            .collect();
        let empty = HashMap::new();
        let inputs: Vec<Input> = members
            .iter()
            .filter(|m| !skip_here.contains(&m.pkgbase))
            .filter_map(|m| {
                srcinfos.get(&m.pkgbase).map(|(_, si)| Input {
                    pkgbase: &m.pkgbase,
                    srcinfo: si,
                    provider_choices: policies.get(&m.pkgbase).unwrap_or(&empty),
                })
            })
            .collect();
        let plan = planner::plan(&idx.index, &app.config.arch, &inputs, &known_aur);

        // Unknown dependencies: look them up on the AUR and onboard them.
        let mut unresolved_by_pkg: HashMap<String, Vec<String>> = HashMap::new();
        for u in &plan.unresolved {
            unresolved_by_pkg.entry(u.pkgbase.clone()).or_default().push(u.dep.clone());
        }
        let mut dead: HashSet<String> = HashSet::new();
        for (pkgbase, deps) in &unresolved_by_pkg {
            for dep in deps {
                let name = repoman_alpm::DepSpec::parse(dep).name;
                let found = app.aur.info(std::slice::from_ref(&name)).await.unwrap_or_default();
                let found = if found.is_empty() {
                    app.aur.search_provides(&name).await.unwrap_or_default()
                } else {
                    found
                };
                match found.len() {
                    0 => {
                        planned.skipped.push(serde_json::json!({"pkgbase": pkgbase, "stream": stream.name, "reason": format!("dependency {dep} not found on the AUR or in the stream")}));
                        dead.insert(pkgbase.clone());
                    }
                    1 => {
                        let base = &found[0].package_base;
                        let kind = if base.ends_with("-git") { "vcs" } else { "release" };
                        if app.store.add_package(base, kind, None)? {
                            tracing::info!(pkgbase = %base, needed_by = %pkgbase, "discovered AUR dependency");
                            app.store.event("package.discovered", Some(base), serde_json::json!({"needed_by": pkgbase, "dep": dep}))?;
                        }
                        app.store.mark_required(base, &stream.name)?;
                        planned.discovered += 1;
                    }
                    _ => {
                        let names: Vec<&str> = found.iter().map(|f| f.package_base.as_str()).collect();
                        planned.skipped.push(serde_json::json!({"pkgbase": pkgbase, "stream": stream.name, "reason": format!("dependency {dep} has several AUR providers {names:?}; set policy.provider_choices")}));
                        dead.insert(pkgbase.clone());
                    }
                }
            }
        }
        if planned.discovered > 0 {
            continue; // the caller loops back to poll the new packages
        }

        // Members whose member dependencies rebuild must rebuild too.
        let mut rebuild_closure: BTreeSet<String> = needs.clone();
        loop {
            let before = rebuild_closure.len();
            for b in &plan.builds {
                if b.aur_deps.iter().any(|d| rebuild_closure.contains(d)) {
                    rebuild_closure.insert(b.pkgbase.clone());
                }
            }
            if rebuild_closure.len() == before {
                break;
            }
        }
        for pb in &rebuild_closure {
            if !needs.contains(pb) && candidate_of.get(pb).cloned().flatten().is_none() {
                if let Some((commit, _)) = srcinfos.get(pb) {
                    let c = app.store.create_candidate(
                        run_id,
                        &NewCandidate {
                            pkgbase: pb,
                            kind: "dependency_rebuild",
                            aur_commit: Some(commit),
                            state: "approved",
                            review: serde_json::json!({"stream": stream.name}),
                            ..Default::default()
                        },
                    )?;
                    candidate_of.insert(pb.clone(), Some(c.id));
                }
            }
        }

        let mut items = Vec::new();
        for b in plan.builds {
            if !rebuild_closure.contains(&b.pkgbase) || dead.contains(&b.pkgbase) {
                continue;
            }
            // Non-member AUR deps must already be published on this stream.
            let missing: Vec<&String> = b
                .aur_deps
                .iter()
                .filter(|d| !member_names.contains(*d))
                .filter(|d| {
                    srcinfos.get(*d).is_none_or(|(_, si)| {
                        !si.pkgnames().iter().all(|n| app.store.live_artifact(&stream.name, n).ok().flatten().is_some())
                    })
                })
                .collect();
            if !missing.is_empty() {
                // Make them members so the next pass plans them.
                for d in &missing {
                    app.store.mark_required(d, &stream.name)?;
                }
                planned.discovered += missing.len();
                continue;
            }
            let (commit, si) = &srcinfos[&b.pkgbase];
            let pkg = packages.iter().find(|p| p.pkgbase == b.pkgbase).unwrap();
            let skip_pgp = pkg.policy.get("pgp").and_then(|v| v.as_str()) == Some("skip");
            let allow_network = pkg.policy.get("allow_network_build").and_then(|v| v.as_bool()) == Some(true);
            if !b.ambiguous_providers.is_empty() {
                tracing::warn!(pkgbase = %b.pkgbase, providers = ?b.ambiguous_providers, "unpinned virtual providers");
            }
            // policy.extra_depends: official packages a PKGBUILD needs but
            // forgot to list (a missing makedepends is the most common AUR
            // build failure). Installed into the build root alongside the
            // declared dependencies.
            let mut install = b.install;
            for extra in pkg
                .policy
                .get("extra_depends")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
            {
                if !install.iter().any(|i| i == extra) {
                    install.push(extra.to_string());
                }
            }
            items.push(BuildItem {
                pkgbase: b.pkgbase.clone(),
                stream: stream.name.clone(),
                aur_commit: commit.clone(),
                candidate_id: candidate_of.get(&b.pkgbase).cloned().flatten(),
                install,
                member_deps: b.aur_deps.into_iter().filter(|d| member_names.contains(d)).collect(),
                pkgnames: si.pkgnames().iter().map(|s| s.to_string()).collect(),
                validpgpkeys: si.base.values("validpgpkeys").into_iter().map(str::to_string).collect(),
                skip_pgp,
                allow_network,
                publish_streams: once_targets
                    .get(&b.pkgbase)
                    .cloned()
                    .unwrap_or_else(|| vec![stream.name.clone()]),
            });
        }
        for c in &plan.cycles {
            planned.skipped.push(serde_json::json!({"pkgbase": c, "stream": stream.name, "reason": "dependency cycle"}));
        }
        planned.per_stream.insert(stream.name.clone(), items);
    }
    Ok(planned)
}

// ---- build ------------------------------------------------------------------------

type BuildKey = (String, String); // (stream, pkgbase)

pub struct BuildOutcome {
    pub results: BTreeMap<BuildKey, Result<Vec<String>, String>>,
    /// Candidates already published incrementally during the build phase.
    pub published_candidates: HashSet<String>,
    pub published_count: usize,
}

/// Which candidates a publish pass covers.
pub struct PublishScope {
    pub only: Option<HashSet<String>>,
    pub exclude: HashSet<String>,
}

impl PublishScope {
    fn covers(&self, item: &BuildItem) -> bool {
        match &item.candidate_id {
            Some(c) => self.only.as_ref().is_none_or(|o| o.contains(c)) && !self.exclude.contains(c),
            // Baseline-only rebuilds have no candidate; the final pass takes them.
            None => self.only.is_none(),
        }
    }
}

async fn build_all(app: &AppState, run_id: &str, planned: &Planned, images: &HashMap<String, ImageRef>) -> Result<BuildOutcome> {
    let mut results: BTreeMap<BuildKey, Result<Vec<String>, String>> = BTreeMap::new();
    let mut published_candidates: HashSet<String> = HashSet::new();
    let mut published_count = 0usize;
    let mut in_flight: HashMap<String, (BuildKey, u32)> = HashMap::new(); // job id -> (key, attempt)
    let mut remaining: Vec<BuildItem> = planned.per_stream.values().flatten().cloned().collect();
    if remaining.is_empty() {
        return Ok(BuildOutcome {
            results,
            published_candidates,
            published_count,
        });
    }
    for stream in planned.per_stream.keys() {
        app.publisher.ensure_staging(run_id, stream).await?;
    }
    for c in remaining.iter().filter_map(|i| i.candidate_id.clone()).collect::<BTreeSet<_>>() {
        app.store.set_candidate_state(&c, "building")?;
    }
    let mut rx = app.hub.subscribe();

    loop {
        // Dispatch everything whose member deps are built on its stream.
        let mut still: Vec<BuildItem> = Vec::new();
        for item in remaining.drain(..) {
            let key = (item.stream.clone(), item.pkgbase.clone());
            let dep_states: Vec<Option<&Result<Vec<String>, String>>> = item
                .member_deps
                .iter()
                .map(|d| results.get(&(item.stream.clone(), d.clone())))
                .collect();
            if dep_states.iter().any(|s| matches!(s, Some(Err(_)))) {
                results.insert(key, Err("a dependency failed to build".into()));
                continue;
            }
            let deps_pending = item
                .member_deps
                .iter()
                .any(|d| planned.per_stream[&item.stream].iter().any(|b| &b.pkgbase == d) && !results.contains_key(&(item.stream.clone(), d.clone())));
            if deps_pending {
                still.push(item);
                continue;
            }
            match submit_build(app, run_id, &item, images).await {
                Ok(job_id) => {
                    in_flight.insert(job_id, (key, 1));
                }
                Err(e) => {
                    results.insert(key, Err(format!("{e:#}")));
                }
            }
        }
        remaining = still;
        if in_flight.is_empty() {
            if !remaining.is_empty() {
                for item in remaining.drain(..) {
                    results.insert((item.stream.clone(), item.pkgbase.clone()), Err("blocked: dependency never built".into()));
                }
            }
            break;
        }

        // Wait for any in-flight job to finish.
        let finished: Vec<String> = {
            let mut done = Vec::new();
            for id in in_flight.keys() {
                if let Some(row) = app.store.get_job(id)? {
                    if row.state.is_terminal() {
                        done.push(id.clone());
                    }
                }
            }
            if done.is_empty() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(15), rx.recv()).await;
                continue;
            }
            done
        };
        for job_id in finished {
            let (key, attempt) = in_flight.remove(&job_id).unwrap();
            let row = app.store.get_job(&job_id)?.context("job vanished")?;
            let item = planned.per_stream[&key.0].iter().find(|b| b.pkgbase == key.1).unwrap();
            match (row.state, row.result) {
                (JobState::Done, Some(JobResult::Build { artifacts, outside_writes, upstream_rev, .. })) => {
                    let mut shas = Vec::new();
                    for a in &artifacts {
                        app.store
                            .insert_artifact(a, Some(&job_id), &item.stream, Some(&item.aur_commit), upstream_rev.as_deref())?;
                        app.publisher
                            .stage(&app.blobs, run_id, &item.stream, &a.sha256, &a.filename)
                            .await
                            .with_context(|| format!("staging {}", a.filename))?;
                        shas.push(a.sha256.clone());
                    }
                    if !outside_writes.is_empty() {
                        app.store.event(
                            "build.outside_writes",
                            Some(&item.pkgbase),
                            serde_json::json!({"stream": item.stream, "paths": outside_writes, "job": job_id}),
                        )?;
                    }
                    tracing::info!(pkgbase = %item.pkgbase, stream = %item.stream, artifacts = shas.len(), "built");
                    results.insert(key, Ok(shas));
                    // Publish as soon as every build of this candidate succeeded,
                    // so progress survives an interrupted run.
                    if let Some(cid) = &item.candidate_id {
                        let complete = planned
                            .per_stream
                            .values()
                            .flatten()
                            .filter(|b| b.candidate_id.as_deref() == Some(cid))
                            .all(|b| matches!(results.get(&(b.stream.clone(), b.pkgbase.clone())), Some(Ok(_))));
                        if complete {
                            let scope = PublishScope {
                                only: Some(HashSet::from([cid.clone()])),
                                exclude: HashSet::new(),
                            };
                            match publish_all(app, run_id, planned, &results, &scope).await {
                                Ok(n) => {
                                    published_count += n;
                                    published_candidates.insert(cid.clone());
                                }
                                Err(e) => tracing::error!(candidate = %cid, error = %format!("{e:#}"), "incremental publish failed; retried at run end"),
                            }
                        }
                    }
                }
                (state, _) => {
                    let err = row.error.unwrap_or_else(|| format!("job ended in state {}", state.as_str()));
                    if attempt < 2 && !err.contains("not found on the AUR") {
                        tracing::warn!(pkgbase = %item.pkgbase, stream = %item.stream, %err, "build failed; retrying once");
                        match submit_build(app, run_id, item, images).await {
                            Ok(job_id) => {
                                in_flight.insert(job_id, (key, attempt + 1));
                            }
                            Err(e) => {
                                results.insert(key, Err(format!("{e:#}")));
                            }
                        }
                    } else {
                        tracing::error!(pkgbase = %item.pkgbase, stream = %item.stream, %err, "build failed");
                        results.insert(key, Err(err));
                    }
                }
            }
        }
    }
    Ok(BuildOutcome {
        results,
        published_candidates,
        published_count,
    })
}

async fn submit_build(app: &AppState, run_id: &str, item: &BuildItem, images: &HashMap<String, ImageRef>) -> Result<String> {
    let stream = app.config.stream(&item.stream).context("unknown stream")?;
    let image = images.get(&item.stream).context("no image for stream")?.clone();
    let mut extra = vec![Repo {
        name: Publisher::staging_repo_name().to_string(),
        servers: vec![format!("{}/staging/{run_id}/{}/$arch", app.config.public_url, item.stream)],
        cache_servers: vec![],
        sig_level: Some("Never".into()),
    }];
    if app.publisher.has_db(&item.stream) {
        extra.push(Repo {
            name: Publisher::published_repo_name(&item.stream),
            servers: vec![format!("{}/repos/{}/$arch", app.config.public_url, item.stream)],
            cache_servers: vec![],
            sig_level: Some("Never".into()),
        });
    }
    let pacman_conf = app.streams.pacman_conf(stream, &extra, &[]).render();
    let pgp_keys = if item.skip_pgp || item.validpgpkeys.is_empty() {
        Vec::new()
    } else {
        app.keys
            .export(&item.validpgpkeys)
            .await
            .with_context(|| format!("PGP keys for {}", item.pkgbase))?
    };
    let job = app.store.create_job_for(
        &JobKind::Build {
            pkgbase: item.pkgbase.clone(),
            stream: item.stream.clone(),
            aur_commit: item.aur_commit.clone(),
            image,
            source_url: format!("{}/api/sources/{}/{}.tar", app.config.public_url, item.pkgbase, item.aur_commit),
            pacman_conf,
            install: item.install.clone(),
            pgp_keys,
            skip_pgp: item.skip_pgp,
            allow_network: item.allow_network,
            upload_url: format!("{}/api/blobs", app.config.public_url),
        },
        Some(&item.pkgbase),
        Some(&item.stream),
        item.candidate_id.as_deref(),
    )?;
    app.hub.notify();
    Ok(job.id)
}

// ---- publish ----------------------------------------------------------------------

async fn publish_all(app: &AppState, _run_id: &str, planned: &Planned, built: &BTreeMap<BuildKey, Result<Vec<String>, String>>, scope: &PublishScope) -> Result<usize> {
    let mut published = 0usize;
    let signing = app.publisher.signing_enabled();
    if !signing {
        tracing::warn!("signing_key not configured; publishing unsigned packages");
    }
    // Sign every built artifact once and collect what goes to which repo: a
    // normal build publishes to its own stream, a build-once item to every
    // member stream of its package.
    let mut per_target: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (stream, items) in &planned.per_stream {
        for item in items {
            if !scope.covers(item) {
                continue;
            }
            let Some(Ok(shas)) = built.get(&(stream.clone(), item.pkgbase.clone())) else {
                continue;
            };
            for sha in shas {
                let art = app.store.get_artifact(sha)?.context("artifact row missing")?;
                if signing {
                    let sig = app.publisher.sign_blob(&app.blobs, sha, &art.filename).await?;
                    let sig_sha = hex::encode(Sha256::digest(std::fs::read(&sig)?));
                    app.store.set_artifact_signature(sha, &sig_sha)?;
                }
                for target in &item.publish_streams {
                    per_target
                        .entry(target.clone())
                        .or_default()
                        .push((sha.clone(), art.filename.clone()));
                }
            }
        }
    }
    for (stream, to_add) in &per_target {
        if to_add.is_empty() {
            continue;
        }
        // Publication rows first, so every file the new database references
        // already resolves; then the database.
        let mut retired_all = Vec::new();
        for (sha, _) in to_add {
            retired_all.extend(app.store.publish(sha, stream)?);
        }
        if app.publisher.has_db(stream) {
            app.publisher.publish(&app.blobs, stream, to_add).await?;
        } else {
            // Missing or renamed database: rebuild it from every live artifact.
            let live: Vec<(String, String)> = app
                .store
                .live_artifacts(stream)?
                .into_iter()
                .map(|a| (a.sha256, a.filename))
                .collect();
            app.publisher.rebuild(&app.blobs, stream, &live).await?;
        }
        published += to_add.len();
        app.store.event("repo.published", Some(stream), serde_json::json!({"count": to_add.len(), "retired": retired_all.len()}))?;

        // Retention.
        for sha in app.store.prune_retired(stream, 2)? {
            if app.store.get_image(&sha)?.is_none() {
                if let Some(a) = app.store.get_artifact(&sha)? {
                    app.publisher.unpublish(stream, &[], std::slice::from_ref(&a.filename)).await?;
                }
                app.store.delete_artifact(&sha)?;
                let _ = app.blobs.delete(&sha);
            }
        }
    }

    // Candidate outcomes and baselines.
    let mut by_candidate: HashMap<String, Vec<(&BuildItem, bool)>> = HashMap::new();
    for items in planned.per_stream.values() {
        for item in items {
            if !scope.covers(item) {
                continue;
            }
            let ok = matches!(built.get(&(item.stream.clone(), item.pkgbase.clone())), Some(Ok(_)));
            if let Some(c) = &item.candidate_id {
                by_candidate.entry(c.clone()).or_default().push((item, ok));
            }
        }
    }
    for (cid, outcomes) in by_candidate {
        let all_ok = outcomes.iter().all(|(_, ok)| *ok);
        let item = outcomes[0].0;
        if all_ok {
            let si = srcinfo_at(app, &item.pkgbase, &item.aur_commit).await.ok();
            let hash = si.as_ref().map(|_| ()).and(None::<String>);
            let text = app.aur.show(&item.pkgbase, &item.aur_commit, ".SRCINFO").await.ok().flatten();
            let hash = text.as_deref().map(srcinfo_hash).or(hash);
            app.store.set_baseline(&item.pkgbase, &item.aur_commit, hash.as_deref(), None)?;
            app.store.set_candidate_state(&cid, "published")?;
        } else {
            let failed: Vec<&str> = outcomes.iter().filter(|(_, ok)| !ok).map(|(i, _)| i.stream.as_str()).collect();
            app.store.decide_candidate(&cid, "failed", "engine", Some(&format!("build failed on {}", failed.join(", "))))?;
        }
    }
    Ok(published)
}
