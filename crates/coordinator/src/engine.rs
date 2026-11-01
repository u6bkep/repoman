//! The update run. One run is active at a time. Each step persists its
//! progress so the UI can show where a run is, and a run that needs the
//! operator parks in `needs_input` until `start_or_resume` is called again.
//!
//! Gate: `update` candidates are classified and lint-checked (`crate::gate`);
//! routine and metadata changes with a low floor risk auto-approve once they
//! have sat on the AUR for the quarantine period and carry no negative
//! signal. Everything else, and every `onboarding` candidate, waits for the
//! operator. `vcs` and `dependency_rebuild` candidates are auto-approved.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use repoman_alpm::pacman_conf::Repo;
use repoman_alpm::srcinfo::Srcinfo;
use repoman_common::clock;
use repoman_common::config::StreamConfig;
use repoman_common::proto::{ImageRef, JobKind, JobResult, JobState};
use repoman_common::proto::ArtifactInfo;
use repoman_store::artifacts::ArtifactRow;
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
        progress(app, run_id, serde_json::json!({"phase": "streams", "detail": "refreshing stream databases"}));
        app.streams.refresh_all().await.context("refreshing streams")?;
        if let Err(e) = app.cache.maintain() {
            tracing::warn!(error = %format!("{e:#}"), "cache maintenance failed");
        }
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
            progress(app, run_id, serde_json::json!({"phase": "images", "detail": format!("checking base image for {}", s.name)}));
            images.insert(s.name.clone(), ensure_image(app, run_id, s).await?);
        }

        step(app, run_id, "poll").await?;
        progress(app, run_id, serde_json::json!({"phase": "poll", "detail": "asking the AUR and git sources for new commits"}));
        poll_aur(app).await?;
        ingest_lists(app).await;

        step(app, run_id, "candidates").await?;
        progress(app, run_id, serde_json::json!({"phase": "candidates"}));
        let probes = create_candidates(app, run_id, &images).await?;
        let rebuilds = soname_rebuilds(app, run_id)?;
        if rebuilds > 0 {
            tracing::info!(count = rebuilds, "soname rebuild candidates created");
        }

        // The gate runs before the upstream probes so that nothing the gate
        // would approve sits in the queue looking like it waits on a human.
        step(app, run_id, "gate").await?;
        progress(app, run_id, serde_json::json!({"phase": "gate"}));
        let auto = auto_approve(app)?;
        if auto > 0 {
            tracing::info!(count = auto, "gate approved routine candidates after quarantine");
        }

        step(app, run_id, "probes").await?;
        run_probes(app, run_id, probes).await?;

        let pending = app.store.candidates_in_states(&["pending"])?;
        if !pending.is_empty() {
            return Ok(RunEnd::NeedsInput(pending.len()));
        }

        step(app, run_id, "plan").await?;
        progress(app, run_id, serde_json::json!({"phase": "plan"}));
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
        progress(app, run_id, serde_json::json!({"phase": "publish"}));
        let scope = PublishScope {
            only: None,
            exclude: built.published_candidates.union(&built.held_candidates).cloned().collect(),
        };
        let published = built.published_count + publish_all(app, run_id, &planned, &built.results, &scope).await?;

        summary = serde_json::json!({
            "streams": planned.per_stream.keys().collect::<Vec<_>>(),
            "builds_planned": planned.per_stream.values().map(|v| v.len()).sum::<usize>(),
            "builds_succeeded": built.results.values().filter(|r| r.is_ok()).count(),
            "builds_failed": built.results.values().filter(|r| r.is_err()).count(),
            "published": published,
            "held_for_review": built.held_candidates.len(),
            "skipped": planned.skipped,
        });
        break;
    }

    step(app, run_id, "cleanup").await?;
    progress(app, run_id, serde_json::json!({"phase": "done"}));
    let _ = app.publisher.remove_staging(run_id);
    let held = app.store.candidates_in_states(&["publish_review"])?;
    if !held.is_empty() {
        // Built artifacts raised flags; the operator decides, then the next
        // run rebuilds and publishes the cleared ones.
        return Ok(RunEnd::NeedsInput(held.len()));
    }
    Ok(RunEnd::Done(summary))
}

// ---- images -------------------------------------------------------------

async fn ensure_image(app: &AppState, run_id: &str, stream: &StreamConfig) -> Result<ImageRef> {
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
    let job = app.store.create_job_for(
        &JobKind::Image {
            stream: stream.name.clone(),
            pacman_conf: app.streams.pacman_conf(stream, &[], &app.cache.servers()).render(),
            packages: app.config.image_packages.clone(),
            keyrings: stream.keyrings.clone(),
            makepkg_conf_extra: app.config.makepkg_conf_extra.clone(),
            upload_url: format!("{}/api/blobs", app.config.public_url),
        },
        None,
        Some(&stream.name),
        None,
        Some(run_id),
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
            package_base_id: None,
            first_submitted: None,
            missing_since: None,
            orphaned_since: None,
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
        let prev = app.store.get_snapshot(&p.pkgbase)?;
        let Some(info) = by_base.get(&p.pkgbase) else {
            missing_from_aur(app, p, prev).await?;
            continue;
        };
        let now = clock::now();
        let changed = prev.as_ref().map(|s| s.last_modified != info.last_modified).unwrap_or(true);
        let mut snap = SnapshotRow {
            pkgbase: p.pkgbase.clone(),
            fetched_at: now.clone(),
            version: Some(info.version.clone()),
            last_modified: info.last_modified,
            maintainer: info.maintainer.clone(),
            submitter: info.submitter.clone(),
            comaintainers: info.co_maintainers.clone(),
            out_of_date: info.out_of_date,
            votes: Some(info.num_votes),
            popularity: Some(info.popularity),
            head_commit: None,
            package_base_id: info.package_base_id,
            first_submitted: Some(info.first_submitted),
            missing_since: None,
            orphaned_since: None,
        };
        if info.maintainer.is_none() {
            // Keep the first time we saw it orphaned; a package that was
            // already orphaned when added is "orphaned since at least" then.
            snap.orphaned_since = prev.as_ref().and_then(|s| s.orphaned_since.clone()).or(Some(now.clone()));
        }
        let need_sync = changed || !app.aur.mirror_path(&p.pkgbase).join("HEAD").exists();
        if need_sync {
            match app.aur.sync(&p.pkgbase).await {
                Ok(head) => snap.head_commit = Some(head),
                Err(e) => tracing::warn!(pkgbase = %p.pkgbase, error = %e, "syncing AUR mirror"),
            }
        }
        if let Some(prev) = &prev {
            aur_transitions(app, p, prev, info, &now)?;
        }
        app.store.upsert_snapshot(&snap)?;
    }
    Ok(())
}

/// The RPC did not list a package we track. The first miss is noted on the
/// snapshot; the second consecutive one becomes a `deleted` signal, with a
/// probe of the git repository (aurweb removes it with the base) as
/// corroboration. Blocking until the operator clears it is deliberate: a
/// base that comes back under the same name is the resubmission case.
async fn missing_from_aur(app: &AppState, p: &PackageRow, prev: Option<SnapshotRow>) -> Result<()> {
    tracing::warn!(pkgbase = %p.pkgbase, "not found on the AUR");
    let Some(mut snap) = prev else {
        app.store.event("aur.missing", Some(&p.pkgbase), serde_json::json!({"first_poll": true}))?;
        return Ok(());
    };
    let now = clock::now();
    if snap.missing_since.is_none() {
        snap.missing_since = Some(now.clone());
        snap.fetched_at = now;
        app.store.upsert_snapshot(&snap)?;
        app.store.event("aur.missing", Some(&p.pkgbase), serde_json::json!({}))?;
        return Ok(());
    }
    if app.store.active_signal(&p.pkgbase, "aur", "deleted")?.is_none() {
        let git_gone = app.aur.sync(&p.pkgbase).await.is_err();
        let payload = serde_json::json!({
            "kind": "deleted",
            "missing_since": snap.missing_since,
            "git_gone": git_gone,
            "last_maintainer": snap.maintainer,
            "last_version": snap.version,
        });
        app.store.add_signal(&p.pkgbase, None, "aur", "medium", &payload)?;
        app.store.event("aur.deleted", Some(&p.pkgbase), payload)?;
    }
    Ok(())
}

/// Both mailing-list feeds; failures are logged, never fatal to a run.
pub async fn ingest_lists(app: &AppState) {
    match ingest_aur_requests(app).await {
        Ok(n) if n > 0 => tracing::info!(new_signals = n, "aur-requests"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "aur-requests feed"),
    }
    match ingest_aur_general(app).await {
        Ok(n) if n > 0 => tracing::info!(new_signals = n, "aur-general"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "aur-general feed"),
    }
}

/// Messages on aur-general that name a tracked package, as `info` signals
/// (one per message and package, deduplicated by guid). Nothing blocks on
/// them; the reviewer and the operator get to read them.
pub async fn ingest_aur_general(app: &AppState) -> Result<usize> {
    let Some(feed) = &app.aurgen else { return Ok(0) };
    let names: Vec<String> = app.store.list_packages()?.into_iter().filter(|p| p.source_url.is_none()).map(|p| p.pkgbase).collect();
    if names.is_empty() {
        return Ok(0);
    }
    let mut added = 0;
    for m in feed.fetch_mentions(&names).await? {
        let existing = app.store.signals_of(&m.pkgbase)?;
        if existing.iter().any(|s| s.source == "aur-general" && s.payload["guid"] == m.guid) {
            continue;
        }
        let payload = serde_json::json!({
            "kind": "mention",
            "subject": m.subject,
            "who": m.who,
            "excerpt": m.excerpt,
            "link": m.link,
            "guid": m.guid,
            "published": m.published,
        });
        app.store.add_signal(&m.pkgbase, None, "aur-general", "info", &payload)?;
        app.store.event("aur_general.mention", Some(&m.pkgbase), payload)?;
        added += 1;
    }
    Ok(added)
}

/// Read the aur-requests feed and record requests against tracked packages
/// as `aur-requests` signals, one per list message (deduplicated by guid).
/// A rejected or closed request clears the signal its filing raised, with
/// the list as the clearing party. Returns how many signals were added.
pub async fn ingest_aur_requests(app: &AppState) -> Result<usize> {
    let Some(feed) = &app.aurreq else { return Ok(0) };
    let items = feed.fetch().await?;
    let mut added = 0;
    // Oldest first so a filing precedes its outcome within one batch.
    for item in items.iter().rev() {
        let Some(_) = app.store.get_package(&item.pkgbase)? else { continue };
        let existing = app.store.signals_of(&item.pkgbase)?;
        if existing.iter().any(|s| s.source == "aur-requests" && s.payload["guid"] == item.guid) {
            continue;
        }
        let kind = match &item.outcome {
            Some(_) => "request_outcome",
            None => "request",
        };
        let payload = serde_json::json!({
            "kind": kind,
            "request": item.kind,
            "prq": item.prq,
            "outcome": item.outcome,
            "who": item.who,
            "body": item.body,
            "link": item.link,
            "guid": item.guid,
            "published": item.published,
        });
        app.store.add_signal(&item.pkgbase, None, "aur-requests", crate::aurreq::severity(item), &payload)?;
        app.store.event("aur_requests.seen", Some(&item.pkgbase), payload)?;
        added += 1;
        if matches!(item.outcome.as_deref(), Some("rejected" | "closed")) {
            for s in existing.iter().filter(|s| {
                s.source == "aur-requests" && s.kind() == Some("request") && s.payload["prq"] == item.prq && s.cleared_at.is_none()
            }) {
                app.store.clear_signal(&s.id, "aur-requests")?;
            }
        }
    }
    Ok(added)
}

/// What changed on the AUR between two polls, as events and signals.
fn aur_transitions(app: &AppState, p: &PackageRow, prev: &SnapshotRow, info: &crate::aur::AurInfo, now: &str) -> Result<()> {
    let id_changed = matches!((prev.package_base_id, info.package_base_id), (Some(a), Some(b)) if a != b);
    let submitted_changed = matches!(prev.first_submitted, Some(a) if a != info.first_submitted);
    if id_changed || submitted_changed {
        let payload = serde_json::json!({
            "kind": "resubmitted",
            "old_id": prev.package_base_id, "new_id": info.package_base_id,
            "old_first_submitted": prev.first_submitted, "new_first_submitted": info.first_submitted,
            "missing_since": prev.missing_since,
            "old_maintainer": prev.maintainer, "new_maintainer": info.maintainer,
        });
        if app.store.active_signal(&p.pkgbase, "aur", "resubmitted")?.is_none() {
            app.store.add_signal(&p.pkgbase, None, "aur", "high", &payload)?;
        }
        app.store.event("aur.resubmitted", Some(&p.pkgbase), payload)?;
    } else if let Some(since) = &prev.missing_since {
        app.store.event("aur.reappeared", Some(&p.pkgbase), serde_json::json!({"missing_since": since}))?;
    }
    match (&prev.maintainer, &info.maintainer) {
        (None, Some(by)) => {
            let payload = serde_json::json!({
                "kind": "adopted",
                "by": by,
                "orphaned_since": prev.orphaned_since,
                "quarantine_days": app.config.adoption_quarantine_days,
            });
            if app.store.active_signal(&p.pkgbase, "aur", "adopted")?.is_none() {
                let expires = time::OffsetDateTime::now_utc() + time::Duration::days(app.config.adoption_quarantine_days as i64);
                app.store.add_signal_expiring(&p.pkgbase, None, "aur", "medium", &payload, Some(&clock::format(expires)))?;
            }
            app.store.event("aur.adopted", Some(&p.pkgbase), payload)?;
        }
        (Some(from), None) => {
            app.store.event("aur.orphaned", Some(&p.pkgbase), serde_json::json!({"from": from, "at": now}))?;
        }
        (Some(from), Some(to)) if from != to => {
            app.store.event("aur.maintainer_changed", Some(&p.pkgbase), serde_json::json!({"from": from, "to": to}))?;
        }
        _ => {}
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

async fn create_candidates(app: &AppState, run_id: &str, images: &HashMap<String, ImageRef>) -> Result<Vec<(PackageRow, String)>> {
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
                    let mut review = serde_json::json!({"note": "no baseline; first review"});
                    let (floor_risk, llm_risk) = match srcinfo_at(app, &p.pkgbase, &head).await {
                        Ok(si) => review_change(app, run_id, p, None, &head, &si, images, &[], false, &mut review).await?,
                        Err(e) => {
                            review["gate_error"] = serde_json::Value::String(e.to_string());
                            (None, None)
                        }
                    };
                    let c = app.store.create_candidate(
                        run_id,
                        &NewCandidate {
                            pkgbase: &p.pkgbase,
                            kind: "onboarding",
                            aur_commit: Some(&head),
                            first_seen_last_modified: Some(snap.last_modified),
                            class: p.provenance_class.as_deref(),
                            floor_risk: floor_risk.as_deref(),
                            state: "pending",
                            review: review.clone(),
                            ..Default::default()
                        },
                    )?;
                    if llm_risk.is_some() {
                        app.store.set_candidate_review(&c.id, None, None, llm_risk.as_deref(), &review)?;
                    }
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
                    // AUR refuses non-fast-forward pushes, so a head that does
                    // not descend from the published baseline means the
                    // repository itself was replaced.
                    let history_rewritten = !app.aur.is_ancestor(&p.pkgbase, &b.aur_commit, &head).await.unwrap_or(true);
                    if history_rewritten {
                        let payload = serde_json::json!({"kind": "history_rewritten", "baseline": b.aur_commit, "head": head});
                        if app.store.active_signal(&p.pkgbase, "aur", "history_rewritten")?.is_none() {
                            app.store.add_signal(&p.pkgbase, None, "aur", "high", &payload)?;
                        }
                        app.store.event("aur.history_rewritten", Some(&p.pkgbase), payload)?;
                    }
                    let mut hashes: Vec<String> = commits.iter().map(|c| c.hash.clone()).collect();
                    if !hashes.contains(&head) {
                        hashes.push(head.clone());
                    }
                    if let Some(sec) = &app.aursec {
                        match sec.lookup(&p.pkgbase, &hashes).await {
                            Ok(found) => {
                                for a in found.into_iter().filter(|a| a.assessment.is_some()) {
                                    let payload = a.assessment.unwrap();
                                    let sev = crate::aursec::severity(&payload);
                                    let existing = app.store.signals_for(&p.pkgbase, Some(&a.commit))?;
                                    if !existing.iter().any(|s| s.source == "aur-security" && s.aur_commit.as_deref() == Some(a.commit.as_str())) {
                                        app.store.add_signal(&p.pkgbase, Some(&a.commit), "aur-security", sev, &payload)?;
                                    }
                                }
                            }
                            Err(e) => tracing::warn!(pkgbase = %p.pkgbase, error = %format!("{e:#}"), "aur-security lookup failed"),
                        }
                    }
                    // Commit-pinned and package-level signals, as they stood
                    // when the candidate was created; the API serves the live
                    // list.
                    let mut signals = Vec::new();
                    for h in &hashes {
                        for s in app.store.signals_for(&p.pkgbase, Some(h))? {
                            if !signals.iter().any(|x: &repoman_store::signals::SignalRow| x.id == s.id) {
                                signals.push(s);
                            }
                        }
                    }
                    if !signals.is_empty() {
                        review["signals"] = serde_json::to_value(&signals)?;
                    }
                    let (floor_risk, llm_risk) = match srcinfo_at(app, &p.pkgbase, &head).await {
                        Ok(si) => review_change(app, run_id, p, Some(b), &head, &si, images, &commits, history_rewritten, &mut review).await?,
                        Err(e) => {
                            review["gate_error"] = serde_json::Value::String(e.to_string());
                            (None, None)
                        }
                    };
                    let c = app.store.create_candidate(
                        run_id,
                        &NewCandidate {
                            pkgbase: &p.pkgbase,
                            kind: "update",
                            aur_commit: Some(&head),
                            first_seen_last_modified: Some(snap.last_modified),
                            class: p.provenance_class.as_deref(),
                            floor_risk: floor_risk.as_deref(),
                            state: "pending",
                            review: review.clone(),
                            ..Default::default()
                        },
                    )?;
                    if llm_risk.is_some() {
                        app.store.set_candidate_review(&c.id, None, None, llm_risk.as_deref(), &review)?;
                    }
                }
            }
            Some(_) => {
                // Baseline is current. VCS packages still need an upstream check,
                // at most once per policy.probe_interval_hours.
                if p.kind == "vcs" && !open.iter().any(|c| c.kind == "vcs") {
                    let interval = p.policy.get("probe_interval_hours").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let due = interval <= 0.0
                        || app
                            .store
                            .last_job_finished(&p.pkgbase, "probe")?
                            .and_then(|t| clock::seconds_between(&t, &clock::now()))
                            .is_none_or(|age| age >= interval * 3600.0);
                    if due {
                        probes.push((p.clone(), head));
                    } else {
                        tracing::debug!(pkgbase = %p.pkgbase, "VCS probe not due yet");
                    }
                }
            }
        }
    }

    Ok(probes)
}

/// What the run is doing, for the UI: replaces the whole progress record.
fn progress(app: &AppState, run_id: &str, p: serde_json::Value) {
    let mut p = p;
    p["at"] = serde_json::Value::String(clock::now());
    if let Err(e) = app.store.set_run_progress(run_id, &p) {
        tracing::warn!(error = %e, "recording run progress");
    }
}

/// Probe VCS upstreams in parallel through workers and create `vcs`
/// candidates for packages whose published build is behind.
async fn run_probes(app: &AppState, run_id: &str, probes: Vec<(PackageRow, String)>) -> Result<()> {
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
        let job = app.store.create_job_for(
            &JobKind::Probe {
                pkgbase: p.pkgbase.clone(),
                sources,
            },
            Some(&p.pkgbase),
            None,
            None,
            Some(run_id),
        )?;
        probe_jobs.push((p.clone(), head.clone(), job.id));
    }
    if !probe_jobs.is_empty() {
        app.hub.notify();
    }
    let total = probe_jobs.len();
    let waiting: Vec<String> = probe_jobs.iter().map(|(p, _, _)| p.pkgbase.clone()).collect();
    let mut done = 0usize;
    progress(app, run_id, serde_json::json!({"phase": "probes", "total": total, "done": done, "waiting": waiting}));
    for (p, head, job_id) in probe_jobs {
        let row = wait_job(app, &job_id).await?;
        done += 1;
        {
            let waiting: Vec<String> = probes
                .iter()
                .map(|(q, _)| q.pkgbase.clone())
                .filter(|pb| app.store.open_candidates(pb).ok().is_some_and(|_| true))
                .collect::<Vec<_>>()
                .into_iter()
                .filter(|_| false)
                .collect();
            let _ = waiting;
        }
        progress(app, run_id, serde_json::json!({"phase": "probes", "total": total, "done": done, "last": p.pkgbase, "worker": row.worker}));
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

// ---- soname rebuilds -------------------------------------------------------------

/// Arch spells a soname provide as `libfoo.so=3-64`; this turns a NEEDED
/// entry `libfoo.so.3` into (`libfoo.so`, `3`).
fn soname_parts(needed: &str) -> Option<(&str, &str)> {
    let i = needed.find(".so.")?;
    Some((&needed[..i + 3], &needed[i + 4..]))
}

/// A published artifact whose linked libraries a stream no longer provides
/// at the same soname version has to be rebuilt: a `soname_rebuild`
/// candidate at the baseline commit, approved, unless the package already
/// has an open candidate. Libraries no stream package provides at all are
/// skipped (bundled, or from the AUR, where the dependency graph handles it).
fn soname_rebuilds(app: &AppState, run_id: &str) -> Result<usize> {
    use repoman_alpm::depspec::DepSpec;
    let suffix = match app.config.arch.as_str() {
        "x86_64" | "aarch64" => "-64",
        _ => "-32",
    };
    let mut created = 0;
    let mut seen: HashSet<String> = HashSet::new();
    for stream in app.config.streams_sorted() {
        let Some(idx) = app.streams.get(&stream.name) else { continue };
        for a in app.store.live_artifacts(&stream.name)? {
            if seen.contains(&a.pkgbase) {
                continue;
            }
            let Some(needed) = a.needed.as_ref().and_then(|n| n.as_array()) else { continue };
            let mut missing = Vec::new();
            for n in needed.iter().filter_map(|v| v.as_str()) {
                let Some((lib, ver)) = soname_parts(n) else { continue };
                if idx.index.satisfiers(&DepSpec::unversioned(lib)).is_empty() {
                    continue; // nothing in the stream provides this library at all
                }
                if !idx.index.satisfies(&DepSpec::parse(&format!("{lib}={ver}{suffix}"))) {
                    missing.push(n.to_string());
                }
            }
            if missing.is_empty() {
                continue;
            }
            seen.insert(a.pkgbase.clone());
            if !app.store.open_candidates(&a.pkgbase)?.is_empty() {
                continue;
            }
            let Some(baseline) = app.store.get_baseline(&a.pkgbase)? else { continue };
            app.store.create_candidate(
                run_id,
                &NewCandidate {
                    pkgbase: &a.pkgbase,
                    kind: "soname_rebuild",
                    aur_commit: Some(&baseline.aur_commit),
                    state: "approved",
                    review: serde_json::json!({"stream": stream.name, "sonames": missing, "note": "stream no longer provides these sonames"}),
                    ..Default::default()
                },
            )?;
            app.store.event("candidate.soname_rebuild", Some(&a.pkgbase), serde_json::json!({"stream": stream.name, "sonames": missing}))?;
            created += 1;
        }
    }
    Ok(created)
}

// ---- gate -----------------------------------------------------------------------

/// Run the deterministic gate and, when configured, the LLM reviewer on one
/// change. Writes `gate`, `gate_error`, and `llm` into `review`; returns the
/// floor and LLM risks for the candidate columns.
#[allow(clippy::too_many_arguments)]
async fn review_change(
    app: &AppState,
    run_id: &str,
    p: &PackageRow,
    baseline: Option<&repoman_store::BaselineRow>,
    head: &str,
    si: &Srcinfo,
    images: &HashMap<String, ImageRef>,
    commits: &[crate::aur::CommitInfo],
    history_rewritten: bool,
    review: &mut serde_json::Value,
) -> Result<(Option<String>, Option<String>)> {
    progress(app, run_id, serde_json::json!({"phase": "candidates", "detail": format!("assessing {}", p.pkgbase)}));
    let a = match crate::gate::assess(app, run_id, &p.pkgbase, baseline, head, si, images, p.source_url.is_some(), history_rewritten).await {
        Ok(a) => a,
        Err(e) => {
            review["gate_error"] = serde_json::Value::String(e.to_string());
            return Ok((None, None));
        }
    };
    review["gate"] = serde_json::to_value(&a.verdict)?;
    if let Some(c) = &a.corroboration {
        review["corroboration"] = serde_json::to_value(c)?;
    }
    let floor = a.verdict.floor_risk.as_str().to_string();
    let Some(model) = app.reviewer.model_for(&a.verdict) else {
        return Ok((Some(floor), None));
    };
    let version_from = review["version_from"].as_str().map(str::to_string);
    // Context beyond the diff: what we already know, and what AUR users said.
    let now = clock::now();
    let signal_lines: Vec<String> = app
        .store
        .signals_of(&p.pkgbase)?
        .iter()
        .filter(|s| s.is_active(&now))
        .map(|s| {
            let what = match s.source.as_str() {
                "aur-requests" => format!(
                    "{} request #{}{} by {}: {}",
                    s.payload["request"].as_str().unwrap_or("?"),
                    s.payload["prq"],
                    s.payload["outcome"].as_str().map(|o| format!(" {o}")).unwrap_or_default(),
                    s.payload["who"].as_str().unwrap_or("?"),
                    s.payload["body"].as_str().unwrap_or("").lines().skip(2).collect::<Vec<_>>().join(" ")
                ),
                "aur-security" => format!("{}: {}", s.payload["verdict"].as_str().unwrap_or("?"), s.payload["explanation"].as_str().unwrap_or("")),
                "aur-general" => format!(
                    "mentioned on the list by {} in \"{}\": {}",
                    s.payload["who"].as_str().unwrap_or("?"),
                    s.payload["subject"].as_str().unwrap_or(""),
                    s.payload["excerpt"].as_str().unwrap_or("")
                ),
                "aur" => format!("{} {}", s.kind().unwrap_or("state change"), s.payload),
                _ => s.payload.to_string(),
            };
            format!("{} [{}]{} {}", s.source, s.severity, s.aur_commit.as_deref().map(|c| format!(" commit {}", &c[..c.len().min(10)])).unwrap_or_default(), what)
        })
        .collect();
    let comments: Vec<crate::aurweb::Comment> = if p.source_url.is_some() {
        Vec::new()
    } else {
        progress(app, run_id, serde_json::json!({"phase": "candidates", "detail": format!("reading AUR comments on {}", p.pkgbase)}));
        match app.aurweb.comments(&p.pkgbase).await {
            Ok(all) => {
                let since = baseline.map(|b| b.published_at.clone());
                let mut keep: Vec<_> = all
                    .into_iter()
                    .filter(|c| c.pinned || since.as_deref().is_none_or(|s| c.sort_key().as_str() > &s[..s.len().min(16)]))
                    .collect();
                if since.is_none() {
                    keep.truncate(10);
                }
                keep.sort_by_key(|c| (!c.pinned, c.sort_key()));
                review["comments"] = serde_json::json!({"shown": keep.len(), "url": app.aurweb.page_url(&p.pkgbase)});
                keep
            }
            Err(e) => {
                tracing::warn!(pkgbase = %p.pkgbase, error = %format!("{e:#}"), "AUR comments unavailable");
                review["comments"] = serde_json::json!({"error": format!("{e:#}")});
                Vec::new()
            }
        }
    };
    progress(app, run_id, serde_json::json!({"phase": "candidates", "detail": format!("LLM review of {} ({model})", p.pkgbase)}));
    let outcome = app
        .reviewer
        .review(
            model,
            &crate::reviewer::ReviewRequest {
                pkgbase: &p.pkgbase,
                kind: if baseline.is_some() { "update" } else { "onboarding" },
                provenance_class: p.provenance_class.as_deref(),
                source_url: p.source_url.as_deref(),
                version_from: version_from.as_deref(),
                version_to: &si.version(),
                maintainers_before: &a.maintainers_before,
                maintainers_now: &a.maintainers_now,
                old_pkgbuild: a.old_pkgbuild.as_deref(),
                new_pkgbuild: &a.new_pkgbuild,
                old_scripts: &a.old_scripts,
                new_scripts: &a.new_scripts,
                commits,
                gate: &a.verdict,
                signals: &signal_lines,
                comments: &comments,
            },
        )
        .await;
    if let Some(e) = &outcome.error {
        tracing::warn!(pkgbase = %p.pkgbase, model, error = %e, "LLM review recorded no verdict");
    }
    tracing::info!(pkgbase = %p.pkgbase, model, risk = ?outcome.risk(), cost_usd = outcome.cost_usd, "LLM review");
    let llm_risk = outcome.risk().map(|r| r.as_str().to_string());
    review["llm"] = serde_json::to_value(&outcome)?;
    Ok((Some(floor), llm_risk))
}

/// Approve pending `update` candidates the deterministic gate is satisfied
/// with: routine (or metadata, where allowed) class, low floor risk, past
/// quarantine by the AUR's own LastModified clock, no negative signal.
fn auto_approve(app: &AppState) -> Result<usize> {
    if !app.config.auto_approve {
        return Ok(0);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut approved = 0;
    for c in app.store.candidates_in_states(&["pending"])? {
        if c.kind != "update" {
            continue;
        }
        let Some(gate) = c.review.get("gate") else {
            continue;
        };
        let class = gate["diff_class"].as_str().unwrap_or("structural");
        let policy = app.store.get_package(&c.pkgbase)?.map(|p| p.policy).unwrap_or_default();
        let metadata_ok = app.config.auto_approve_metadata && policy.get("auto_metadata").and_then(|v| v.as_bool()).unwrap_or(true);
        let class_ok = class == "routine" || (class == "metadata" && metadata_ok);
        if !class_ok || c.floor_risk.as_deref() != Some("low") {
            continue;
        }
        if c.llm_risk.as_deref().is_some_and(|r| r != "low") {
            continue;
        }
        let days = policy.get("quarantine_days").and_then(|v| v.as_u64()).unwrap_or(app.config.quarantine_days);
        let Some(seen) = c.first_seen_last_modified else {
            continue;
        };
        let age_days = (now - seen) / 86_400;
        if now - seen < days as i64 * 86_400 {
            tracing::debug!(pkgbase = %c.pkgbase, age_days, days, "candidate still in quarantine");
            continue;
        }
        if app.store.has_negative_signal(&c.pkgbase, c.aur_commit.as_deref())? {
            continue;
        }
        let range: Vec<String> = c.review["commits"].as_array().into_iter().flatten().filter_map(|k| k["hash"].as_str().map(str::to_string)).collect();
        let mut blocked = false;
        for h in &range {
            if app.store.has_negative_signal(&c.pkgbase, Some(h))? {
                blocked = true;
                break;
            }
        }
        if blocked {
            tracing::info!(pkgbase = %c.pkgbase, "negative signal on an intermediate commit; not auto-approving");
            continue;
        }
        app.store.decide_candidate(
            &c.id,
            "approved",
            "gate",
            Some(&format!("auto-approved: {class} change, floor risk low, {age_days} days on the AUR")),
        )?;
        app.store.event("candidate.auto_approved", Some(&c.pkgbase), serde_json::json!({"id": c.id, "class": class}))?;
        approved += 1;
    }
    Ok(approved)
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
            for tool in si.vcs_tools(&app.config.arch) {
                if !install.iter().any(|i| i == tool) {
                    install.push(tool.to_string());
                }
            }
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
    /// Candidates whose builds succeeded but whose artifacts raised flags;
    /// parked in `publish_review` for the operator, not published.
    pub held_candidates: HashSet<String>,
    pub published_count: usize,
}

/// Paths whose appearance in a package is a new privileged surface on the
/// installing system: anything executed by root or at boot, auth policy,
/// loader configuration, package manager hooks.
const PRIVILEGED_PREFIXES: &[&str] = &[
    "etc/sudoers",
    "etc/polkit-1/",
    "usr/share/polkit-1/rules.d/",
    "usr/lib/systemd/system/",
    "etc/systemd/system/",
    "usr/lib/systemd/system-generators/",
    "usr/lib/systemd/user/",
    "usr/share/libalpm/hooks/",
    "etc/pacman.d/hooks/",
    "etc/profile.d/",
    "etc/ld.so.conf.d/",
    "etc/ld.so.preload",
    "etc/cron",
    "usr/lib/sysusers.d/",
    "usr/lib/tmpfiles.d/",
    "etc/xdg/autostart/",
    "usr/share/dbus-1/system-services/",
    "etc/dbus-1/system.d/",
    "usr/lib/security/",
    "etc/pam.d/",
    "usr/lib/modules-load.d/",
    "etc/modules-load.d/",
    "usr/lib/udev/rules.d/",
    "etc/udev/rules.d/",
    "usr/lib/NetworkManager/dispatcher.d/",
    "etc/NetworkManager/dispatcher.d/",
];

fn is_privileged(path: &str) -> bool {
    let p = path.trim_start_matches('/');
    PRIVILEGED_PREFIXES.iter().any(|pre| p.starts_with(pre))
}

/// Post-build flags for one artifact against what the stream currently
/// serves for the same pkgname. Nothing to compare against on a first
/// publication: the PKGBUILD review covered it.
pub fn artifact_flags(prev: Option<&ArtifactRow>, a: &ArtifactInfo) -> Vec<String> {
    let mut flags = Vec::new();
    // setuid/setgid is rare enough to surface even on a first publication.
    let prev_setuid: HashSet<&str> = prev.map(|p| p.setuid_paths.iter().map(String::as_str).collect()).unwrap_or_default();
    let new_setuid: Vec<&str> = a.setuid_paths.iter().map(String::as_str).filter(|p| !prev_setuid.contains(p)).collect();
    if !new_setuid.is_empty() {
        flags.push(format!("{}: setuid/setgid files {new_setuid:?}", a.pkgname));
    }
    let Some(prev) = prev else {
        return flags;
    };
    if a.has_install_script && !prev.has_install {
        flags.push(format!("{}: install script appeared", a.pkgname));
    }
    let before: HashSet<&str> = prev.paths.iter().map(String::as_str).collect();
    let mut added: Vec<&str> = a
        .paths
        .iter()
        .map(String::as_str)
        .filter(|p| !before.contains(p) && is_privileged(p))
        .collect();
    added.sort();
    if !added.is_empty() {
        let shown: Vec<&str> = added.iter().take(8).copied().collect();
        flags.push(format!(
            "{}: new privileged paths {shown:?}{}",
            a.pkgname,
            if added.len() > 8 { format!(" (+{} more)", added.len() - 8) } else { String::new() }
        ));
    }
    flags
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
    let mut held_candidates: HashSet<String> = HashSet::new();
    let mut flags: HashMap<String, Vec<String>> = HashMap::new(); // candidate -> publish flags
    let mut published_count = 0usize;
    let mut in_flight: HashMap<String, (BuildKey, u32)> = HashMap::new(); // job id -> (key, attempt)
    let mut remaining: Vec<BuildItem> = planned.per_stream.values().flatten().cloned().collect();
    if remaining.is_empty() {
        return Ok(BuildOutcome {
            results,
            published_candidates,
            held_candidates,
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
            match submit_build(app, run_id, &item, images, 0).await {
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
                let running: Vec<serde_json::Value> = in_flight
                    .iter()
                    .filter_map(|(id, (key, _))| {
                        let row = app.store.get_job(id).ok().flatten()?;
                        Some(serde_json::json!({"job": id, "pkgbase": key.1, "stream": key.0, "worker": row.worker, "state": row.state.as_str(), "since": row.started_at.or(Some(row.queued_at))}))
                    })
                    .collect();
                progress(
                    app,
                    run_id,
                    serde_json::json!({
                        "phase": "build",
                        "total": planned.per_stream.values().map(|v| v.len()).sum::<usize>(),
                        "done": results.values().filter(|r| r.is_ok()).count(),
                        "failed": results.values().filter(|r| r.is_err()).count(),
                        "running": running,
                        "queued": remaining.len(),
                    }),
                );
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
                (JobState::Done, Some(JobResult::Build { artifacts, outside_writes, upstream_rev, net_attempts })) => {
                    let mut shas = Vec::new();
                    if let Some(cid) = &item.candidate_id {
                        let mut f = Vec::new();
                        for a in &artifacts {
                            let prev = app.store.live_artifact(&item.stream, &a.pkgname)?;
                            f.extend(artifact_flags(prev.as_ref(), a).into_iter().map(|x| format!("{}: {x}", item.stream)));
                        }
                        if !outside_writes.is_empty() {
                            f.push(format!("{}: build wrote outside the build tree: {:?}", item.stream, outside_writes.iter().take(6).collect::<Vec<_>>()));
                        }
                        if net_attempts > 0 && !item.allow_network {
                            f.push(format!("{}: {net_attempts} network attempts during the offline build", item.stream));
                        }
                        if !f.is_empty() {
                            flags.entry(cid.clone()).or_default().extend(f);
                        }
                    }
                    for a in &artifacts {
                        app.store
                            .insert_artifact(a, Some(&job_id), &item.stream, Some(&item.aur_commit), upstream_rev.as_deref())?;
                        app.store.set_artifact_needed(&a.sha256, &serde_json::json!(a.needed))?;
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
                        if complete
                            && let Some(f) = flags.get(cid)
                            && !f.is_empty()
                        {
                            // A human looks at the built artifacts before they
                            // reach the repo, unless they already did for this
                            // candidate and approved from publish_review.
                            let cleared = app
                                .store
                                .get_candidate(cid)?
                                .is_some_and(|c| c.review["publish_cleared"].as_bool() == Some(true));
                            if !cleared {
                                let mut review = app.store.get_candidate(cid)?.map(|c| c.review).unwrap_or_default();
                                review["publish_flags"] = serde_json::to_value(f)?;
                                app.store.set_candidate_review(cid, None, None, None, &review)?;
                                app.store.decide_candidate(cid, "publish_review", "engine", Some("built; artifact flags need a decision"))?;
                                app.store.event("candidate.publish_review", Some(&item.pkgbase), serde_json::json!({"id": cid, "flags": f}))?;
                                tracing::warn!(pkgbase = %item.pkgbase, flags = ?f, "holding publication for review");
                                held_candidates.insert(cid.clone());
                                continue;
                            }
                        }
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
                        match submit_build(app, run_id, item, images, attempt + 1).await {
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
        held_candidates,
        published_count,
    })
}

/// Memory limit for a build: the largest recorded peak times the headroom
/// factor, rounded up to 256 MiB and never under 1 GiB; the configured
/// default when the package has never built here. Each retry doubles it.
fn predict_memory(app: &AppState, pkgbase: &str, attempt: u32) -> u64 {
    let peaks = app.store.recent_build_peaks(pkgbase, 5).unwrap_or_default();
    let max_rss = peaks.iter().filter_map(|(rss, _)| *rss).max();
    let base = match max_rss {
        Some(rss) => {
            let predicted = (rss as f64 * app.config.build_memory_headroom) as u64;
            let step = 256u64 << 20;
            (predicted.div_ceil(step) * step).max(1 << 30)
        }
        None => app.config.default_build_memory_bytes,
    };
    base.saturating_mul(1u64 << attempt.min(4))
}

async fn submit_build(app: &AppState, run_id: &str, item: &BuildItem, images: &HashMap<String, ImageRef>, attempt: u32) -> Result<String> {
    let stream = app.config.stream(&item.stream).context("unknown stream")?;
    let memory_limit = predict_memory(app, &item.pkgbase, attempt);
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
    let pacman_conf = app.streams.pacman_conf(stream, &extra, &app.cache.servers()).render();
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
            memory_limit: Some(memory_limit),
        },
        Some(&item.pkgbase),
        Some(&item.stream),
        item.candidate_id.as_deref(),
        Some(run_id),
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
            let snap = app.store.get_snapshot(&item.pkgbase)?;
            let maintainers = crate::gate::maintainer_set(snap.as_ref());
            app.store.set_baseline(&item.pkgbase, &item.aur_commit, hash.as_deref(), None, &maintainers)?;
            app.store.set_candidate_state(&cid, "published")?;
        } else {
            let failed: Vec<&str> = outcomes.iter().filter(|(_, ok)| !ok).map(|(i, _)| i.stream.as_str()).collect();
            app.store.decide_candidate(&cid, "failed", "engine", Some(&format!("build failed on {}", failed.join(", "))))?;
        }
    }
    Ok(published)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pkgname: &str, paths: &[&str], install: bool) -> ArtifactInfo {
        ArtifactInfo {
            sha256: "x".into(),
            filename: format!("{pkgname}-1-1-x86_64.pkg.tar.zst"),
            size: 1,
            pkgname: pkgname.into(),
            pkgbase: pkgname.into(),
            pkgver: "1-1".into(),
            arch: "x86_64".into(),
            depends: vec![],
            provides: vec![],
            paths: paths.iter().map(|p| p.to_string()).collect(),
            has_install_script: install,
            setuid_paths: vec![],
            needed: vec![],
            buildinfo: serde_json::json!({}),
        }
    }

    #[test]
    fn soname_parsing() {
        assert_eq!(soname_parts("libfoo.so.3"), Some(("libfoo.so", "3")));
        assert_eq!(soname_parts("libc.so.6"), Some(("libc.so", "6")));
        assert_eq!(soname_parts("libbar.so.1.2"), Some(("libbar.so", "1.2")));
        assert_eq!(soname_parts("libnover.so"), None);
    }

    fn prev(paths: &[&str], install: bool) -> ArtifactRow {
        ArtifactRow {
            sha256: "p".into(),
            job_id: None,
            pkgbase: "foo".into(),
            pkgname: "foo".into(),
            pkgver: "0-1".into(),
            arch: "x86_64".into(),
            stream: "s1".into(),
            filename: "foo-0-1-x86_64.pkg.tar.zst".into(),
            size: 1,
            sig_sha256: None,
            aur_commit: None,
            upstream_rev: None,
            depends: vec![],
            provides: vec![],
            paths: paths.iter().map(|p| p.to_string()).collect(),
            has_install: install,
            setuid_paths: vec![],
            buildinfo: serde_json::json!({}),
            needed: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn artifact_flags_catch_new_privileged_surface() {
        let p = prev(&["usr/bin/foo", "usr/lib/systemd/system/foo.service"], false);
        assert!(artifact_flags(None, &info("foo", &["etc/sudoers.d/foo"], true)).is_empty(), "first publication");
        assert!(artifact_flags(Some(&p), &info("foo", &["usr/bin/foo", "usr/lib/systemd/system/foo.service", "usr/share/doc/x"], false)).is_empty());
        let f = artifact_flags(Some(&p), &info("foo", &["usr/bin/foo", "etc/sudoers.d/foo", "usr/share/libalpm/hooks/foo.hook"], true));
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[0].contains("install script appeared"));
        assert!(f[1].contains("etc/sudoers.d/foo") && f[1].contains("foo.hook"));
        let mut suid = info("foo", &["usr/bin/foo"], false);
        suid.setuid_paths = vec!["usr/bin/foo".into()];
        assert_eq!(artifact_flags(None, &suid).len(), 1, "setuid shows even on first publication");
        let mut p2 = p.clone();
        p2.setuid_paths = vec!["usr/bin/foo".into()];
        assert!(artifact_flags(Some(&p2), &suid).is_empty(), "unchanged setuid is not re-flagged");
    }
}
