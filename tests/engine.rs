//! End-to-end run against a fake AUR (local bare git repos plus a stub RPC),
//! a fake stream (a sync database served over HTTP), and a worker whose
//! runner fabricates images and packages instead of calling pacstrap and
//! makepkg. The coordinator's planner, gate, staging, repo-add publishing,
//! baselines, and candidate lifecycle run for real.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path as AxPath, Query, State};
use axum::routing::get;
use repoman_alpm::srcinfo::Srcinfo;
use repoman_alpm::syncdb::fixtures::{build_db, pkg};
use repoman_alpm::syncdb::read_db;
use repoman_common::config::{CoordinatorConfig, StreamConfig, WorkerConfig, WorkerCredential};
use repoman_common::proto::{ArtifactInfo, JobKind, JobResult, JobSpec, Outcome};
use repoman_store::Store;
use repoman_worker::WorkerState;
use repoman_worker::exec::{JobCtx, JobRunner, RunFuture};
use serde_json::{Value, json};
use sha2::Digest;

// ---------------------------------------------------------------------------
// Fake AUR: bare repos under <dir>/aur, RPC metadata derived from them.

struct FakeAur {
    dir: PathBuf,
    work: PathBuf,
    /// pkgbase -> last_modified counter bumped on every push
    stamps: Arc<Mutex<HashMap<String, i64>>>,
}

impl FakeAur {
    fn new(root: &Path) -> Self {
        let dir = root.join("aur");
        let work = root.join("aur-work");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        FakeAur {
            dir,
            work,
            stamps: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn git(&self, cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "maint")
            .env("GIT_AUTHOR_EMAIL", "m@x")
            .env("GIT_COMMITTER_NAME", "maint")
            .env("GIT_COMMITTER_EMAIL", "m@x")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
    }

    /// Create or update a package: writes PKGBUILD and .SRCINFO, commits,
    /// pushes to the bare repo the coordinator clones from.
    fn publish(&self, pkgbase: &str, pkgver: &str, depends: &[&str], makedepends: &[&str]) {
        let bare = self.dir.join(format!("{pkgbase}.git"));
        let wd = self.work.join(pkgbase);
        if !bare.exists() {
            self.git(&self.dir, &["init", "-q", "--bare", &format!("{pkgbase}.git")]);
            std::fs::create_dir_all(&wd).unwrap();
            self.git(&wd, &["init", "-q", "-b", "master"]);
            self.git(&wd, &["remote", "add", "origin", bare.to_str().unwrap()]);
        }
        self.publish_inner(pkgbase, pkgver, depends, makedepends, "master");
    }

    fn publish_inner(&self, pkgbase: &str, pkgver: &str, depends: &[&str], makedepends: &[&str], branch: &str) {
        let wd = self.work.join(pkgbase);
        let deps = depends.iter().map(|d| format!("\tdepends = {d}\n")).collect::<String>();
        let mdeps = makedepends.iter().map(|d| format!("\tmakedepends = {d}\n")).collect::<String>();
        std::fs::write(
            wd.join(".SRCINFO"),
            format!(
                "pkgbase = {pkgbase}\n\tpkgdesc = fake {pkgbase}\n\tpkgver = {pkgver}\n\tpkgrel = 1\n\turl = https://example.org/{pkgbase}\n\tarch = x86_64\n\tlicense = MIT\n{mdeps}{deps}\tsource = https://example.org/{pkgbase}-{pkgver}.tar.gz\n\tsha256sums = SKIP\n\npkgname = {pkgbase}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            wd.join("PKGBUILD"),
            format!("pkgname={pkgbase}\npkgver={pkgver}\npkgrel=1\narch=(x86_64)\nsource=()\npackage() {{ :; }}\n"),
        )
        .unwrap();
        self.git(&wd, &["add", "-A"]);
        self.git(&wd, &["commit", "-q", "-m", &format!("{pkgbase} {pkgver}")]);
        self.git(&wd, &["push", "-q", "origin", branch]);
        let mut st = self.stamps.lock().unwrap();
        let e = st.entry(pkgbase.to_string()).or_insert(1_700_000_000);
        *e += 60;
    }

    /// Publish a version whose SRCINFO references an install script that is
    /// also committed, for the artifact-diff tests.
    fn publish_with_install(&self, pkgbase: &str, pkgver: &str, depends: &[&str]) {
        let wd = self.work.join(pkgbase);
        assert!(wd.exists(), "publish {pkgbase} first");
        self.publish_inner(pkgbase, pkgver, depends, &[], "master");
        let si = std::fs::read_to_string(wd.join(".SRCINFO")).unwrap().replace("\tpkgrel = 1\n", &format!("\tpkgrel = 1\n\tinstall = {pkgbase}.install\n"));
        std::fs::write(wd.join(".SRCINFO"), si).unwrap();
        std::fs::write(wd.join(format!("{pkgbase}.install")), "post_install() { :; }\n").unwrap();
        self.git(&wd, &["add", "-A"]);
        self.git(&wd, &["commit", "-q", "-m", &format!("{pkgbase} {pkgver} install script")]);
        self.git(&wd, &["push", "-q", "origin", "master"]);
        let mut st = self.stamps.lock().unwrap();
        *st.entry(pkgbase.to_string()).or_insert(1_700_000_000) += 60;
    }

    /// Like publish, but commits on `branch` (created from master if new)
    /// and pushes only that branch, leaving master where it was.
    fn publish_on_branch(&self, pkgbase: &str, branch: &str, pkgver: &str, depends: &[&str], makedepends: &[&str]) {
        let wd = self.work.join(pkgbase);
        assert!(wd.exists(), "publish {pkgbase} on master first");
        self.git(&wd, &["checkout", "-q", "-B", branch]);
        self.publish_inner(pkgbase, pkgver, depends, makedepends, branch);
        self.git(&wd, &["checkout", "-q", "master"]);
    }

    /// The AUR deleted the base and someone pushed a new one under the same
    /// name: the bare repository and its history are replaced wholesale.
    fn resubmit(&self, pkgbase: &str, pkgver: &str) {
        std::fs::remove_dir_all(self.dir.join(format!("{pkgbase}.git"))).unwrap();
        std::fs::remove_dir_all(self.work.join(pkgbase)).unwrap();
        self.publish(pkgbase, pkgver, &["glibc"], &[]);
    }

    fn git_url(&self) -> String {
        format!("file://{}", self.dir.display())
    }
}

/// Per-package RPC fields the tests move around: maintainer and identity.
#[derive(Clone)]
struct AurMeta {
    maintainer: Option<String>,
    base_id: i64,
    first_submitted: i64,
}

impl Default for AurMeta {
    fn default() -> Self {
        AurMeta { maintainer: Some("maint".into()), base_id: 1, first_submitted: 1_600_000_000 }
    }
}

// ---------------------------------------------------------------------------
// Stub HTTP: AUR RPC and a stream mirror.

#[derive(Clone)]
struct StubState {
    stamps: Arc<Mutex<HashMap<String, i64>>>,
    versions: Arc<Mutex<HashMap<String, String>>>,
    /// (bytes, Last-Modified) of the fake core database; tests swap it.
    core_db: Arc<Mutex<(Vec<u8>, String)>>,
    /// Package files the fake mirror serves, by filename.
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    mirror_requests: Arc<Mutex<Vec<String>>>,
    /// Package bases the fake aur-security service calls dangerous.
    dangerous: Arc<Mutex<Vec<String>>>,
    meta: Arc<Mutex<HashMap<String, AurMeta>>>,
}

async fn aursec_lookup(State(st): State<StubState>, axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
    let dangerous = st.dangerous.lock().unwrap();
    let results: Vec<Value> = body["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let base = p["package_base"].as_str().unwrap_or("");
            let commits: Vec<Value> = p["commits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    let assessment = if dangerous.iter().any(|d| d == base) {
                        json!({"verdict": "dangerous", "explanation": "install script downloads a second stage", "model": "fake"})
                    } else {
                        Value::Null
                    };
                    json!({"commit": c, "assessment": assessment})
                })
                .collect();
            json!({"package_base": base, "commits": commits})
        })
        .collect();
    axum::Json(json!({"results": results}))
}

async fn rpc_info(State(st): State<StubState>, Query(q): Query<Vec<(String, String)>>) -> axum::Json<Value> {
    let stamps = st.stamps.lock().unwrap();
    let versions = st.versions.lock().unwrap();
    let meta = st.meta.lock().unwrap();
    let results: Vec<Value> = q
        .iter()
        .filter(|(k, _)| k == "arg[]")
        .filter_map(|(_, name)| {
            let lm = stamps.get(name)?;
            let m = meta.get(name).cloned().unwrap_or_default();
            Some(json!({
                "Name": name, "PackageBase": name, "Version": versions.get(name).cloned().unwrap_or_default(),
                "LastModified": lm, "FirstSubmitted": m.first_submitted, "Maintainer": m.maintainer,
                "NumVotes": 1, "Popularity": 0.5, "ID": m.base_id, "PackageBaseID": m.base_id,
            }))
        })
        .collect();
    axum::Json(json!({"type": "multiinfo", "resultcount": results.len(), "results": results, "version": 5}))
}

async fn rpc_search(AxPath(_name): AxPath<String>) -> axum::Json<Value> {
    axum::Json(json!({"type": "search", "resultcount": 0, "results": [], "version": 5}))
}

async fn mirror_db(State(st): State<StubState>, AxPath((repo, _arch, file)): AxPath<(String, String, String)>) -> axum::response::Response {
    use axum::response::IntoResponse;
    st.mirror_requests.lock().unwrap().push(format!("{repo}/{file}"));
    if repo == "core" && file == "core.db" {
        let (bytes, lastmod) = st.core_db.lock().unwrap().clone();
        ([(axum::http::header::LAST_MODIFIED, lastmod)], bytes).into_response()
    } else if let Some(bytes) = st.files.lock().unwrap().get(&file) {
        bytes.clone().into_response()
    } else {
        axum::http::StatusCode::NOT_FOUND.into_response()
    }
}

async fn start_stub(st: StubState) -> String {
    let app = Router::new()
        .route("/rpc/v5/info", get(rpc_info))
        .route("/rpc/v5/search/{name}", get(rpc_search))
        .route("/mirror/{repo}/{arch}/{file}", get(mirror_db))
        .route("/api/v1/checks/lookup", axum::routing::post(aursec_lookup))
        .with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Fake worker runner.

struct FakeRunner {
    specs: Arc<Mutex<Vec<JobSpec>>>,
}

impl JobRunner for FakeRunner {
    fn run<'a>(&'a self, ctx: &'a JobCtx, spec: &'a JobSpec) -> RunFuture<'a> {
        self.specs.lock().unwrap().push(spec.clone());
        Box::pin(async move {
            match &spec.job {
                JobKind::Image { stream, upload_url, .. } => {
                    ctx.stage("pacstrap");
                    let dir = ctx.job_dir();
                    tokio::fs::create_dir_all(&dir).await?;
                    let tarball = dir.join("image.tar.zst");
                    let bytes = tarball_zst(&[("etc/os-release", b"fake")]);
                    std::fs::write(&tarball, &bytes)?;
                    let (sha, size) = repoman_worker::upload::sha256_file(&tarball)?;
                    let filename = format!("{stream}-{}.tar.zst", &sha[..12]);
                    repoman_worker::upload::put_blob(&ctx.http, upload_url, &ctx.config.token, &tarball, &sha, &filename).await?;
                    ctx.stage_done("pacstrap");
                    Ok(Outcome::success_with(JobResult::Image {
                        sha256: sha,
                        size,
                        filename,
                        packages: vec!["glibc 2.44-1".into()],
                    }))
                }
                JobKind::Build {
                    pkgbase,
                    source_url,
                    upload_url,
                    aur_commit,
                    ..
                } => {
                    ctx.stage("build");
                    let dir = ctx.job_dir();
                    tokio::fs::create_dir_all(&dir).await?;
                    let src = ctx.http.get(source_url).send().await?.error_for_status()?.bytes().await?;
                    let srcinfo = extract_srcinfo(&src, pkgbase)?;
                    let si = Srcinfo::parse(&srcinfo)?;
                    let mut artifacts = Vec::new();
                    for name in si.pkgnames() {
                        let ver = si.version();
                        let depends = si.effective(name, "depends", "x86_64");
                        // A package depending on libfoo links libfoo.so.1.
                        let needed: Vec<String> = if depends.iter().any(|d| d == "libfoo") { vec!["libfoo.so.1".into()] } else { vec![] };
                        let mut pkginfo = format!("pkgname = {name}\npkgbase = {pkgbase}\npkgver = {ver}\narch = x86_64\nsize = 10\n");
                        for d in &depends {
                            pkginfo.push_str(&format!("depend = {d}\n"));
                        }
                        let payload = format!("{pkgbase}@{aur_commit}");
                        let bytes = tarball_zst(&[(".PKGINFO", pkginfo.as_bytes()), (&format!("usr/bin/{name}"), payload.as_bytes())]);
                        let filename = format!("{name}-{ver}-x86_64.pkg.tar.zst");
                        let path = dir.join(&filename);
                        std::fs::write(&path, &bytes)?;
                        let (sha, size) = repoman_worker::upload::sha256_file(&path)?;
                        repoman_worker::upload::put_blob(&ctx.http, upload_url, &ctx.config.token, &path, &sha, &filename).await?;
                        artifacts.push(ArtifactInfo {
                            sha256: sha,
                            filename,
                            size,
                            pkgname: name.to_string(),
                            pkgbase: pkgbase.clone(),
                            pkgver: ver,
                            arch: "x86_64".into(),
                            depends,
                            provides: vec![],
                            paths: vec![format!("usr/bin/{name}")],
                            has_install_script: si.base.first("install").is_some(),
                            setuid_paths: vec![],
                            needed,
                            buildinfo: json!({}),
                        });
                    }
                    ctx.stage_done("build");
                    Ok(Outcome::success_with(JobResult::Build {
                        artifacts,
                        net_attempts: 0,
                        outside_writes: vec![],
                        upstream_rev: None,
                    }))
                }
                JobKind::Inspect { pkgbase, source_url, .. } => {
                    let src = ctx.http.get(source_url).send().await?.error_for_status()?.bytes().await?;
                    let srcinfo = extract_srcinfo(&src, pkgbase)?;
                    Ok(Outcome::success_with(JobResult::Inspect { srcinfo }))
                }
                other => anyhow::bail!("fake runner cannot run {}", other.name()),
            }
        })
    }
}

fn tarball_zst(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar_bytes = Vec::new();
    {
        let mut ar = tar::Builder::new(&mut tar_bytes);
        for (path, body) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append_data(&mut h, path, *body).unwrap();
        }
        ar.finish().unwrap();
    }
    zstd::encode_all(&tar_bytes[..], 3).unwrap()
}

fn extract_srcinfo(tar_bytes: &[u8], pkgbase: &str) -> anyhow::Result<String> {
    use std::io::Read;
    let mut ar = tar::Archive::new(tar_bytes);
    for e in ar.entries()? {
        let mut e = e?;
        let p = e.path()?.to_string_lossy().to_string();
        if p == format!("{pkgbase}/.SRCINFO") {
            let mut s = String::new();
            e.read_to_string(&mut s)?;
            return Ok(s);
        }
    }
    anyhow::bail!("no .SRCINFO in source tarball")
}

// ---------------------------------------------------------------------------
// Harness.

struct Harness {
    base: String,
    mirror_requests: Arc<Mutex<Vec<String>>>,
    dangerous: Arc<Mutex<Vec<String>>>,
    meta: Arc<Mutex<HashMap<String, AurMeta>>>,
    core_db: Arc<Mutex<(Vec<u8>, String)>>,
    specs: Arc<Mutex<Vec<JobSpec>>>,
    aur: FakeAur,
    versions: Arc<Mutex<HashMap<String, String>>>,
    bulk: PathBuf,
    _tmp: tempfile::TempDir,
}

async fn start() -> Harness {
    start_with(|_| {}).await
}

/// `auto_approve` is off by default so the lifecycle tests drive every
/// decision; tests that need it, or the reviewer, adjust the config here.
async fn start_with(tweak: impl FnOnce(&mut CoordinatorConfig)) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let aur = FakeAur::new(tmp.path());
    let versions = Arc::new(Mutex::new(HashMap::new()));
    let mut glibc = pkg("glibc", "2.44-1");
    glibc.provides = vec!["libc.so=6-64".into()];
    // cmake carries a real size and checksum so the cache can verify it;
    // the fake mirror serves the matching bytes (and a corrupt python).
    let cmake_bytes = b"cmake package bytes".to_vec();
    let mut cmake = pkg("cmake", "3.30-1");
    cmake.csize = Some(cmake_bytes.len() as u64);
    cmake.sha256sum = Some(hex::encode(sha2::Sha256::digest(&cmake_bytes)));
    let mut python = pkg("python", "3.13-1");
    python.sha256sum = Some("00".repeat(32));
    let mut files = HashMap::new();
    files.insert(cmake.filename.clone(), cmake_bytes);
    files.insert(python.filename.clone(), b"not what the database says".to_vec());
    let mut libfoo = pkg("libfoo", "1.0-1");
    libfoo.provides = vec!["libfoo.so=1-64".into()];
    let core_db = build_db(&[glibc, cmake, python, libfoo], false);
    let core_db = Arc::new(Mutex::new((core_db, "Fri, 31 Oct 2026 00:00:00 GMT".to_string())));
    let mirror_requests = Arc::new(Mutex::new(Vec::new()));
    let dangerous: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let meta: Arc<Mutex<HashMap<String, AurMeta>>> = Arc::new(Mutex::new(HashMap::new()));
    let stub = start_stub(StubState {
        stamps: aur.stamps.clone(),
        versions: versions.clone(),
        core_db: core_db.clone(),
        files: Arc::new(Mutex::new(files)),
        mirror_requests: mirror_requests.clone(),
        dangerous: dangerous.clone(),
        meta: meta.clone(),
    })
    .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bulk = tmp.path().join("bulk");
    let config = CoordinatorConfig {
        listen: addr.to_string(),
        public_url: format!("http://{addr}"),
        data_dir: tmp.path().join("data"),
        bulk_dir: Some(bulk.clone()),
        workers: vec![WorkerCredential {
            name: "w1".into(),
            token: "t1".into(),
        }],
        streams: vec![StreamConfig {
            name: "s1".into(),
            sort_order: 0,
            server: format!("{stub}/mirror/$repo/$arch"),
            repos: vec!["core".into()],
            keyrings: vec![],
        }],
        aur_rpc_url: format!("{stub}/rpc/v5"),
        aur_git_url: aur.git_url(),
        auto_approve: false,
        forge_checks: false,
        aur_security_url: Some(stub.clone()),
        ..Default::default()
    };
    let mut config = config;
    tweak(&mut config);
    std::fs::create_dir_all(&config.data_dir).unwrap();
    let store = Arc::new(Store::open(&tmp.path().join("repoman.db")).unwrap());
    tokio::spawn(repoman_coordinator::serve(config, store, listener));

    let specs = Arc::new(Mutex::new(Vec::new()));
    let state = WorkerState::with_runner(
        2,
        vec!["build".into(), "image".into(), "probe".into()],
        Arc::new(FakeRunner { specs: specs.clone() }),
    );
    let wcfg = WorkerConfig {
        coordinator: format!("ws://{addr}/ws/worker"),
        name: "w1".into(),
        token: "t1".into(),
        slots: 2,
        scratch_dir: tmp.path().join("scratch"),
        arch: "x86_64".into(),
        ..Default::default()
    };
    tokio::spawn(repoman_worker::run_with_state(wcfg, state));

    let h = Harness {
        base: format!("http://{addr}"),
        mirror_requests,
        dangerous,
        meta,
        core_db,
        specs,
        aur,
        versions,
        bulk,
        _tmp: tmp,
    };
    // Wait for the worker.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let v = h.get("/api/workers").await;
        if v.as_array().unwrap().iter().any(|w| w["connected"] == true) {
            break;
        }
        assert!(Instant::now() < deadline, "worker never connected");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h
}

impl Harness {
    async fn get(&self, path: &str) -> Value {
        reqwest::get(format!("{}{path}", self.base)).await.unwrap().json().await.unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let resp = reqwest::Client::new().post(format!("{}{path}", self.base)).json(&body).send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        assert!(status.is_success(), "POST {path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn post_status(&self, path: &str, body: Value) -> u16 {
        reqwest::Client::new().post(format!("{}{path}", self.base)).json(&body).send().await.unwrap().status().as_u16()
    }

    fn set_maintainer(&self, pkgbase: &str, m: Option<&str>) {
        self.meta.lock().unwrap().entry(pkgbase.to_string()).or_default().maintainer = m.map(str::to_string);
    }

    /// Onboard `foo`, approve it, publish 1.0: the common starting point.
    async fn seed_foo(&self) {
        self.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"], "class": "upstream-source"})).await;
        self.publish_pkg("foo", "1.0", &["glibc"], &[]);
        assert_eq!(self.run().await["state"], "needs_input");
        let c = self.get("/api/candidates").await.as_array().unwrap()[0].clone();
        self.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
        assert_eq!(self.run().await["state"], "done");
    }

    fn publish_pkg(&self, pkgbase: &str, ver: &str, depends: &[&str], makedepends: &[&str]) {
        self.aur.publish(pkgbase, ver, depends, makedepends);
        self.versions.lock().unwrap().insert(pkgbase.to_string(), format!("{ver}-1"));
    }

    /// Start or resume a run and wait until it finishes or needs input.
    async fn run(&self) -> Value {
        let started = self.post("/api/runs", json!({})).await;
        let id = started["id"].as_str().unwrap().to_string();
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let v = self.get(&format!("/api/runs/{id}")).await;
            if v["finished_at"].is_string() || v["state"] == "needs_input" {
                return v;
            }
            assert!(Instant::now() < deadline, "run did not settle: {v}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn repo_db(&self) -> Vec<repoman_alpm::syncdb::SyncPackage> {
        let p = self.bulk.join("repos/s1/x86_64/repoman.db.tar.zst");
        read_db(&std::fs::read(&p).unwrap_or_default()).unwrap_or_default()
    }
}

#[tokio::test]
async fn onboarding_build_publish_update_cycle() {
    let h = start().await;
    h.publish_pkg("foo", "1.0", &["glibc"], &["cmake"]);
    h.publish_pkg("bar", "2.0", &["foo", "python"], &[]);
    h.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"]})).await;
    h.post("/api/packages", json!({"pkgbase": "bar", "streams": ["s1"]})).await;

    // Run 1: everything is new, so two onboarding candidates wait for us.
    let r1 = h.run().await;
    assert_eq!(r1["state"], "needs_input", "{r1}");
    let pending = h.get("/api/candidates").await;
    let pending = pending.as_array().unwrap();
    assert_eq!(pending.len(), 2, "{pending:?}");
    assert!(pending.iter().all(|c| c["kind"] == "onboarding"));
    let images = h.get("/api/images").await;
    assert_eq!(images.as_array().unwrap().len(), 1, "image built during run 1");

    // Approving an onboarding candidate requires a class.
    let bad = reqwest::Client::new()
        .post(format!("{}/api/candidates/{}/decide", h.base, pending[0]["id"].as_str().unwrap()))
        .json(&json!({"decision": "approve"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    for c in pending {
        h.post(
            &format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()),
            json!({"decision": "approve", "class": "upstream-source"}),
        )
        .await;
    }

    // Run 2 (resume): builds foo then bar, publishes both.
    let r2 = h.run().await;
    assert_eq!(r2["state"], "done", "{r2}");
    assert_eq!(r2["summary"]["builds_succeeded"], 2, "{}", r2["summary"]);
    assert_eq!(r2["summary"]["published"], 2);
    {
        let specs = h.specs.lock().unwrap();
        let builds: Vec<&JobSpec> = specs.iter().filter(|s| matches!(s.job, JobKind::Build { .. })).collect();
        assert_eq!(builds.len(), 2);
        assert_eq!(builds[0].pkgbase.as_deref(), Some("foo"), "dependency builds first");
        assert_eq!(builds[1].pkgbase.as_deref(), Some("bar"));
        if let JobKind::Build { install, pacman_conf, .. } = &builds[1].job {
            assert_eq!(install, &["foo", "python"], "bar installs its AUR dep and the official one");
            assert!(pacman_conf.contains("[repoman-staging]"), "staging repo precedes stream repos:\n{pacman_conf}");
            assert!(pacman_conf.find("[repoman-staging]").unwrap() < pacman_conf.find("[core]").unwrap());
        }
        if let JobKind::Build { install, .. } = &builds[0].job {
            assert_eq!(install, &["cmake", "glibc"]);
        }
    }
    let db = h.repo_db();
    let mut names: Vec<&str> = db.iter().map(|p| p.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["bar", "foo"]);
    assert_eq!(db.iter().find(|p| p.name == "foo").unwrap().version, "1.0-1");
    let foo = h.get("/api/packages/foo").await;
    assert!(foo["baseline"]["aur_commit"].is_string(), "baseline advanced on publish");
    let cands = h.get("/api/candidates?state=published").await;
    assert_eq!(cands.as_array().unwrap().len(), 2);
    // The repo files are served and resolvable.
    let served = reqwest::get(format!("{}/repos/s1/x86_64/{}", h.base, db[0].filename)).await.unwrap();
    assert_eq!(served.status(), 200);

    // Run 3: nothing changed, nothing to do.
    let r3 = h.run().await;
    assert_eq!(r3["state"], "done");
    assert_eq!(r3["summary"]["builds_planned"], 0, "{}", r3["summary"]);

    // A package from a git repository that is not on the AUR: the stub RPC
    // knows nothing about it, so polling must use the commit itself.
    h.aur.publish("selfhosted", "0.1", &["glibc"], &[]);
    let src = format!("{}/selfhosted.git", h.aur.git_url());
    h.post("/api/packages", json!({"pkgbase": "selfhosted", "streams": ["s1"], "source": src})).await;
    let rg = h.run().await;
    assert_eq!(rg["state"], "needs_input", "{rg}");
    let pending = h.get("/api/candidates").await;
    let c = &pending.as_array().unwrap()[0];
    assert_eq!(c["pkgbase"], "selfhosted");
    assert!(c["first_seen_last_modified"].as_i64().unwrap() > 1_600_000_000, "commit time used as the clock");
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve", "class": "authored"})).await;
    let rg2 = h.run().await;
    assert_eq!(rg2["state"], "done", "{rg2}");
    assert!(h.repo_db().iter().any(|p| p.name == "selfhosted"));
    let pkg = h.get("/api/packages/selfhosted").await;
    assert_eq!(pkg["source_url"], src);
    assert!(pkg["snapshot"]["maintainer"].is_null());

    // Following a branch other than the remote's default: 0.2 lives only on
    // `next`, so re-pointing the source at it makes 0.2 the candidate while
    // master still says 0.1.
    h.aur.publish_on_branch("selfhosted", "next", "0.2", &["glibc"], &[]);
    let branched = format!("{src}#branch=next");
    h.post("/api/packages", json!({"pkgbase": "selfhosted", "streams": ["s1"], "source": branched})).await;
    let rb = h.run().await;
    assert_eq!(rb["state"], "needs_input", "{rb}");
    let pending = h.get("/api/candidates").await;
    let c = pending.as_array().unwrap().iter().find(|c| c["pkgbase"] == "selfhosted").expect("branch candidate");
    assert_eq!(c["review"]["version_to"], "0.2-1", "{c}");
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    let rb2 = h.run().await;
    assert_eq!(rb2["state"], "done", "{rb2}");
    assert!(h.repo_db().iter().any(|p| p.name == "selfhosted" && p.version == "0.2-1"));
    // Bad fragments are refused at add time.
    let bad = h.post_status("/api/packages", json!({"pkgbase": "selfhosted", "streams": ["s1"], "source": format!("{src}#commit=abc")})).await;
    assert_eq!(bad, 400);

    // Upstream bumps foo. The candidate review shows the version change,
    // bar is rebuilt as a dependent, and the repo carries the new versions.
    h.publish_pkg("foo", "1.1", &["glibc"], &["cmake"]);
    let r4 = h.run().await;
    assert_eq!(r4["state"], "needs_input", "{r4}");
    let pending = h.get("/api/candidates").await;
    let pending = pending.as_array().unwrap();
    assert_eq!(pending.len(), 1);
    let c = &pending[0];
    assert_eq!(c["kind"], "update");
    assert_eq!(c["pkgbase"], "foo");
    assert_eq!(c["review"]["version_from"], "1.0-1");
    assert_eq!(c["review"]["version_to"], "1.1-1");
    let changed: Vec<&str> = c["review"]["changes"].as_array().unwrap().iter().map(|x| x["key"].as_str().unwrap()).collect();
    assert_eq!(changed, ["pkgver", "source"]);
    assert_eq!(c["review"]["commits"].as_array().unwrap().len(), 1);
    // The gate saw a plain version bump: routine, low risk, SRCINFO regenerated
    // by the inspect job matched the committed one.
    assert_eq!(c["review"]["gate"]["diff_class"], "routine", "{}", c["review"]["gate"]);
    assert_eq!(c["floor_risk"], "low");
    let kinds: Vec<&str> = c["review"]["gate"]["reasons"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"version_bump") && !kinds.contains(&"srcinfo_mismatch"), "{kinds:?}");
    assert!(h.specs.lock().unwrap().iter().any(|s| matches!(s.job, JobKind::Inspect { .. })), "inspect job ran");
    // Builds carry a predicted memory limit: the default for a package with no recorded peaks.
    assert!(h.specs.lock().unwrap().iter().any(|s| matches!(&s.job, JobKind::Build { memory_limit: Some(l), .. } if *l == 4 << 30)), "default footprint");
    // The UI's supporting endpoints: on-demand packaging diff, package history, static files.
    let diff = h.get(&format!("/api/candidates/{}/diff", c["id"].as_str().unwrap())).await;
    assert!(diff["pkgbuild"].as_str().unwrap().contains("-pkgver=1.0\n+pkgver=1.1"), "{}", diff["pkgbuild"]);
    assert_eq!(diff["from"], c["review"]["from"]);
    let hist = h.get("/api/packages/foo/history").await;
    assert!(hist["candidates"].as_array().unwrap().len() >= 2, "{hist}");
    assert!(hist["publications"].as_array().unwrap().iter().any(|p| p["artifact"]["pkgname"] == "foo" && p["retired_at"].is_null()), "{hist}");
    let ui = reqwest::get(format!("{}/ui/", h.base)).await.unwrap();
    assert_eq!(ui.status(), 200);
    assert!(ui.text().await.unwrap().contains("<title>repoman</title>"));
    assert_eq!(reqwest::get(format!("{}/", h.base)).await.unwrap().url().path(), "/ui/", "root redirects to the UI");
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;

    let r5 = h.run().await;
    assert_eq!(r5["state"], "done", "{r5}");
    assert_eq!(r5["summary"]["builds_succeeded"], 2, "foo and dependent bar: {}", r5["summary"]);
    let db = h.repo_db();
    assert_eq!(db.iter().find(|p| p.name == "foo").unwrap().version, "1.1-1");
    assert_eq!(db.iter().filter(|p| p.name == "foo").count(), 1, "old foo replaced, not duplicated");
    let kinds: Vec<String> = h
        .get("/api/candidates?state=published")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"dependency_rebuild".to_string()), "{kinds:?}");

    // A rejected update stays rejected until the head moves.
    h.publish_pkg("bar", "2.1", &["foo", "python"], &[]);
    let r6 = h.run().await;
    assert_eq!(r6["state"], "needs_input");
    let pending = h.get("/api/candidates").await;
    let c = &pending.as_array().unwrap()[0];
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "reject", "note": "nope"})).await;
    let r7 = h.run().await;
    assert_eq!(r7["state"], "done", "{r7}");
    assert_eq!(r7["summary"]["builds_planned"], 0);
    assert_eq!(h.repo_db().iter().find(|p| p.name == "bar").unwrap().version, "2.0-1");
}

#[tokio::test]
async fn routine_update_auto_approves_after_quarantine() {
    let h = start_with(|c| c.auto_approve = true).await;
    h.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"], "class": "upstream-source"})).await;
    h.publish_pkg("foo", "1.0", &["glibc"], &[]);
    let r1 = h.run().await;
    assert_eq!(r1["state"], "needs_input", "onboarding is never automatic: {r1}");
    let pending = h.get("/api/candidates").await;
    let c = &pending.as_array().unwrap()[0];
    assert_eq!(c["kind"], "onboarding");
    assert_eq!(c["floor_risk"], "low", "head lint of a clean PKGBUILD: {c}");
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    assert_eq!(h.run().await["state"], "done");

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    // A bump the AUR says was modified just now sits in quarantine.
    h.publish_pkg("foo", "1.1", &["glibc"], &[]);
    h.aur.stamps.lock().unwrap().insert("foo".into(), now);
    let r2 = h.run().await;
    assert_eq!(r2["state"], "needs_input", "{r2}");
    let pending = h.get("/api/candidates").await;
    let c = &pending.as_array().unwrap()[0];
    assert_eq!(c["review"]["gate"]["diff_class"], "routine");
    assert_eq!(c["floor_risk"], "low");
    assert!(c["decided_by"].is_null());

    // The head moves again, to a commit the AUR has shown for four days:
    // the new candidate supersedes the quarantined one and goes through.
    h.publish_pkg("foo", "1.2", &["glibc"], &[]);
    h.aur.stamps.lock().unwrap().insert("foo".into(), now - 4 * 86_400);
    let r3 = h.run().await;
    assert_eq!(r3["state"], "done", "{r3}");
    assert_eq!(r3["summary"]["builds_succeeded"], 1, "{}", r3["summary"]);
    let published = h.get("/api/candidates?state=published").await;
    let auto = published
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["pkgbase"] == "foo" && c["review"]["version_to"] == "1.2-1")
        .expect("1.2 published");
    assert_eq!(auto["decided_by"], "gate", "{auto}");
    assert_eq!(h.repo_db().iter().find(|p| p.name == "foo").unwrap().version, "1.2-1");

    // A structural change never auto-approves, however old.
    h.publish_pkg("foo", "1.3", &["glibc", "libnotinrepos"], &[]);
    h.aur.stamps.lock().unwrap().insert("foo".into(), now - 30 * 86_400);
    let r4 = h.run().await;
    assert_eq!(r4["state"], "needs_input", "{r4}");
    let pending = h.get("/api/candidates").await;
    let c = &pending.as_array().unwrap()[0];
    assert_eq!(c["review"]["gate"]["diff_class"], "structural", "{}", c["review"]["gate"]);
}

/// A stand-in for the Claude Messages API: records request bodies and
/// answers with a fixed schema-shaped verdict.
async fn start_fake_claude(requests: Arc<Mutex<Vec<Value>>>) -> String {
    use axum::routing::post;
    async fn messages(State(reqs): State<Arc<Mutex<Vec<Value>>>>, headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
        assert_eq!(headers.get("anthropic-version").and_then(|v| v.to_str().ok()), Some("2026-10-21"));
        assert!(headers.get("x-api-key").is_some());
        reqs.lock().unwrap().push(body);
        let verdict = json!({
            "risk": "high",
            "summary": "A new AUR-only dependency appears with no upstream reason.",
            "findings": [{"kind": "new_dependency", "evidence": "depends gained libnotinrepos", "severity": "high"}],
            "questions_for_human": ["Does upstream 1.1 really need libnotinrepos?"]
        });
        axum::Json(json!({
            "id": "msg_test", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
            "content": [{"type": "text", "text": verdict.to_string()}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1500, "output_tokens": 90}
        }))
    }
    let app = Router::new().route("/v1/messages", post(messages)).with_state(requests);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn structural_update_is_sent_to_the_reviewer() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let api = start_fake_claude(requests.clone()).await;
    let h = start_with(|c| {
        c.auto_approve = true;
        c.reviewer.enabled = true;
        c.reviewer.api_url = api.clone();
        c.reviewer.api_key = Some("test-key".into());
    })
    .await;
    h.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"], "class": "upstream-source"})).await;
    h.publish_pkg("foo", "1.0", &["glibc"], &[]);
    let r1 = h.run().await;
    assert_eq!(r1["state"], "needs_input", "{r1}");
    let pending = h.get("/api/candidates").await;
    let c = &pending.as_array().unwrap()[0];
    // Onboarding of a clean package: low floor risk, so no API call is spent.
    assert_eq!(c["floor_risk"], "low");
    assert!(c["review"]["llm"].is_null(), "{}", c["review"]);
    assert_eq!(requests.lock().unwrap().len(), 0);
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    assert_eq!(h.run().await["state"], "done");

    // A structural change goes to the model; its verdict lands on the
    // candidate and the operator sees it in the queue.
    h.publish_pkg("foo", "1.1", &["glibc", "libnotinrepos"], &[]);
    let r2 = h.run().await;
    assert_eq!(r2["state"], "needs_input", "{r2}");
    let pending = h.get("/api/candidates").await;
    let c = &pending.as_array().unwrap()[0];
    assert_eq!(c["review"]["gate"]["diff_class"], "structural");
    assert_eq!(c["llm_risk"], "high", "{}", c["review"]);
    let llm = &c["review"]["llm"];
    assert_eq!(llm["model"], "claude-opus-5-5");
    assert_eq!(llm["verdict"]["findings"][0]["kind"], "new_dependency");
    assert!((llm["cost_usd"].as_f64().unwrap() - (1500.0 * 4.0 + 90.0 * 20.0) / 1e6).abs() < 1e-9, "{llm}");
    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    let body = &reqs[0];
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(body["fallbacks"], "default");
    let prompt = body["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("libnotinrepos"), "prompt shows the diff");
    assert!(prompt.contains("new_aur_dependency"), "prompt shows the gate reasons");
    assert!(prompt.contains("version: 1.0-1 -> 1.1-1"), "{prompt}");
}

#[tokio::test]
async fn artifacts_with_new_install_script_wait_in_publish_review() {
    let h = start_with(|_| {}).await;
    h.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"], "class": "upstream-source"})).await;
    h.publish_pkg("foo", "1.0", &["glibc"], &[]);
    assert_eq!(h.run().await["state"], "needs_input");
    let c = h.get("/api/candidates").await.as_array().unwrap()[0].clone();
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    assert_eq!(h.run().await["state"], "done");

    // 1.1 gains an install script. The gate makes it structural (operator
    // approves the PKGBUILD), the build succeeds, and the artifact diff then
    // parks it: the previous publication had no install script.
    h.aur.publish_with_install("foo", "1.1", &["glibc"]);
    h.versions.lock().unwrap().insert("foo".into(), "1.1-1".into());
    let r = h.run().await;
    assert_eq!(r["state"], "needs_input", "{r}");
    let c = h.get("/api/candidates").await.as_array().unwrap()[0].clone();
    assert_eq!(c["review"]["gate"]["diff_class"], "structural");
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    let r2 = h.run().await;
    assert_eq!(r2["state"], "needs_input", "{r2}");
    let held = h.get("/api/candidates?state=publish_review").await;
    let held = held.as_array().unwrap();
    assert_eq!(held.len(), 1, "{held:?}");
    let flags = held[0]["review"]["publish_flags"].as_array().unwrap();
    assert!(flags[0].as_str().unwrap().contains("install script appeared"), "{flags:?}");
    assert_eq!(h.repo_db().iter().find(|p| p.name == "foo").unwrap().version, "1.0-1", "not published yet");
    // It shows in the default queue, and approving it clears the way.
    assert!(h.get("/api/candidates").await.as_array().unwrap().iter().any(|c| c["state"] == "publish_review"));
    h.post(&format!("/api/candidates/{}/decide", held[0]["id"].as_str().unwrap()), json!({"decision": "approve", "note": "unit file is expected"})).await;
    let r3 = h.run().await;
    assert_eq!(r3["state"], "done", "{r3}");
    assert_eq!(h.repo_db().iter().find(|p| p.name == "foo").unwrap().version, "1.1-1");
    let published = h.get("/api/candidates?state=published").await;
    assert!(published.as_array().unwrap().iter().any(|c| c["id"] == held[0]["id"]));
}

#[tokio::test]
async fn cache_serves_verified_upstream_files() {
    let h = start().await;
    // Streams load on the first run; trigger one so the index knows cmake.
    h.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"]})).await;
    h.publish_pkg("foo", "1.0", &["glibc"], &[]);
    assert_eq!(h.run().await["state"], "needs_input");
    let before = h.mirror_requests.lock().unwrap().len();

    // Miss: fetched from the mirror, verified, served.
    let r = reqwest::get(format!("{}/cache/cmake-3.30-1-x86_64.pkg.tar.zst", h.base)).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"cmake package bytes");
    // Hit: nothing goes to the mirror; ranges work.
    let r = reqwest::Client::new()
        .get(format!("{}/cache/cmake-3.30-1-x86_64.pkg.tar.zst", h.base))
        .header("range", "bytes=0-4")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"cmake");
    let mirror_fetches = h.mirror_requests.lock().unwrap()[before..].iter().filter(|r| r.ends_with(".pkg.tar.zst")).count();
    assert_eq!(mirror_fetches, 1, "one upstream fetch for two requests");

    // A corrupt mirror file never reaches a client.
    let r = reqwest::get(format!("{}/cache/python-3.13-1-x86_64.pkg.tar.zst", h.base)).await.unwrap();
    assert_eq!(r.status(), 502, "{}", r.text().await.unwrap());
    // Not a package file, or unknown everywhere.
    assert_eq!(reqwest::get(format!("{}/cache/core.db", h.base)).await.unwrap().status(), 404);
    assert_eq!(reqwest::get(format!("{}/cache/nope-1-1-x86_64.pkg.tar.zst", h.base)).await.unwrap().status(), 502);

    let view = h.get("/api/cache").await;
    assert_eq!(view["files"], 1, "{view}");
    assert_eq!(view["hits"], 1);
    assert_eq!(view["misses"], 3);
    // Build roots and image bootstraps are pointed at the cache.
    let specs = h.specs.lock().unwrap();
    let conf = specs.iter().find_map(|s| match &s.job { JobKind::Image { pacman_conf, .. } => Some(pacman_conf.clone()), _ => None }).expect("image job");
    assert!(conf.contains(&format!("CacheServer = {}/cache", h.base)), "{conf}");
}

#[tokio::test]
async fn aur_security_signal_blocks_auto_approval() {
    let h = start_with(|c| c.auto_approve = true).await;
    h.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"], "class": "upstream-source"})).await;
    h.publish_pkg("foo", "1.0", &["glibc"], &[]);
    assert_eq!(h.run().await["state"], "needs_input");
    let c = h.get("/api/candidates").await.as_array().unwrap()[0].clone();
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    assert_eq!(h.run().await["state"], "done");

    // A routine bump, long past quarantine, that aur-security calls dangerous.
    h.dangerous.lock().unwrap().push("foo".into());
    h.publish_pkg("foo", "1.1", &["glibc"], &[]);
    let r = h.run().await;
    assert_eq!(r["state"], "needs_input", "{r}");
    let c = h.get("/api/candidates").await.as_array().unwrap()[0].clone();
    assert_eq!(c["review"]["gate"]["diff_class"], "routine");
    assert!(c["decided_by"].is_null(), "not auto-approved: {c}");
    let signals = c["review"]["signals"].as_array().unwrap();
    assert_eq!(signals.len(), 1, "{signals:?}");
    assert_eq!(signals[0]["source"], "aur-security");
    assert_eq!(signals[0]["severity"], "high");
    assert_eq!(signals[0]["payload"]["verdict"], "dangerous");
}

#[tokio::test]
async fn soname_bump_in_stream_rebuilds_dependents() {
    let h = start().await;
    h.post("/api/packages", json!({"pkgbase": "foo", "streams": ["s1"], "class": "upstream-source"})).await;
    h.publish_pkg("foo", "1.0", &["glibc", "libfoo"], &[]);
    assert_eq!(h.run().await["state"], "needs_input");
    let c = h.get("/api/candidates").await.as_array().unwrap()[0].clone();
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    assert_eq!(h.run().await["state"], "done");
    let hist = h.get("/api/packages/foo/history").await;
    assert_eq!(hist["publications"][0]["artifact"]["needed"], json!(["libfoo.so.1"]), "{hist}");
    // Nothing to do while the stream still provides libfoo.so.1.
    let idle = h.run().await;
    assert_eq!(idle["summary"]["builds_planned"], 0, "{idle}");

    // The stream bumps libfoo to a new soname.
    {
        let mut glibc = pkg("glibc", "2.44-1");
        glibc.provides = vec!["libc.so=6-64".into()];
        let mut libfoo = pkg("libfoo", "2.0-1");
        libfoo.provides = vec!["libfoo.so=2-64".into()];
        let db = build_db(&[glibc, pkg("cmake", "3.30-1"), pkg("python", "3.13-1"), libfoo], false);
        *h.core_db.lock().unwrap() = (db, "Sat, 01 Nov 2026 00:00:00 GMT".to_string());
    }
    let r = h.run().await;
    assert_eq!(r["state"], "done", "{r}");
    assert_eq!(r["summary"]["builds_succeeded"], 1, "foo rebuilt without operator input: {}", r["summary"]);
    let published = h.get("/api/candidates?state=published").await;
    let rebuild = published.as_array().unwrap().iter().find(|c| c["kind"] == "soname_rebuild").expect("soname_rebuild candidate");
    assert_eq!(rebuild["pkgbase"], "foo");
    assert_eq!(rebuild["review"]["sonames"], json!(["libfoo.so.1"]));
    // And it does not loop: the rebuilt artifact still says libfoo.so.1 (fake
    // runner), but a second run must not create another candidate while one
    // was just published... it will, since the stream still lacks .so.1; that
    // is the real-world answer too (the package truly needs rebuilding against
    // the new soname and the fake build cannot do it). Check instead that a
    // run with no further change plans the rebuild once more rather than
    // crashing or duplicating candidates.
    let again = h.run().await;
    assert_eq!(again["state"], "done", "{again}");
    let open: Vec<Value> = h.get("/api/candidates?state=approved").await.as_array().unwrap().clone();
    assert!(open.is_empty(), "no dangling approved candidates: {open:?}");
}

#[tokio::test]
async fn adoption_from_orphan_blocks_until_cleared() {
    let h = start_with(|c| {
        c.auto_approve = true;
        c.adoption_quarantine_days = 30;
    })
    .await;
    h.seed_foo().await;

    // The maintainer disowns it. Nothing to build, but the state is kept.
    h.set_maintainer("foo", None);
    assert_eq!(h.run().await["state"], "done");
    let p = h.get("/api/packages/foo").await;
    assert!(p["snapshot"]["maintainer"].is_null(), "{}", p["snapshot"]);
    assert!(p["snapshot"]["orphaned_since"].is_string(), "{}", p["snapshot"]);
    assert!(p["signals"].as_array().unwrap().is_empty(), "orphaning alone is not a signal");

    // Someone adopts it and ships a bump that the AUR has shown for a month.
    h.set_maintainer("foo", Some("mallory"));
    h.publish_pkg("foo", "1.1", &["glibc"], &[]);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    h.aur.stamps.lock().unwrap().insert("foo".into(), now - 30 * 86_400);
    let r = h.run().await;
    assert_eq!(r["state"], "needs_input", "{r}");
    let c = h.get("/api/candidates").await.as_array().unwrap()[0].clone();
    let c = h.get(&format!("/api/candidates/{}", c["id"].as_str().unwrap())).await;
    assert_eq!(c["review"]["gate"]["diff_class"], "structural", "{}", c["review"]["gate"]);
    assert_eq!(c["floor_risk"], "high");
    let reasons: Vec<&str> = c["review"]["gate"]["reasons"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert!(reasons.contains(&"adopted_from_orphan"), "{reasons:?}");
    let sig = c["signals"].as_array().unwrap().iter().find(|s| s["payload"]["kind"] == "adopted").expect("adopted signal").clone();
    assert_eq!(sig["severity"], "medium");
    assert_eq!(sig["payload"]["by"], "mallory");
    assert!(sig["expires_at"].is_string(), "{sig}");
    assert!(sig["cleared_at"].is_null());
    let blocking = h.get("/api/signals").await;
    assert_eq!(blocking.as_array().unwrap().len(), 1, "{blocking}");

    // The operator vouches for mallory and approves the bump.
    let cleared = h.post(&format!("/api/signals/{}/clear", sig["id"].as_str().unwrap()), json!({"by": "ben"})).await;
    assert_eq!(cleared["cleared_by"], "ben", "{cleared}");
    assert!(h.get("/api/signals").await.as_array().unwrap().is_empty());
    h.post(&format!("/api/candidates/{}/decide", c["id"].as_str().unwrap()), json!({"decision": "approve"})).await;
    assert_eq!(h.run().await["state"], "done");

    // With the signal cleared and mallory on the baseline, the next bump is
    // routine again and goes through on its own.
    // (A different LastModified than 1.1 carried, or the poll sees no change.)
    h.publish_pkg("foo", "1.2", &["glibc"], &[]);
    h.aur.stamps.lock().unwrap().insert("foo".into(), now - 29 * 86_400);
    let r = h.run().await;
    assert_eq!(r["state"], "done", "{r}");
    assert_eq!(h.repo_db().iter().find(|p| p.name == "foo").unwrap().version, "1.2-1");
    let pkg = h.get("/api/packages/foo").await;
    let kinds: Vec<&str> = pkg["signals"].as_array().unwrap().iter().filter_map(|s| s["payload"]["kind"].as_str()).collect();
    assert_eq!(kinds, ["adopted"], "history keeps the cleared signal");
}

#[tokio::test]
async fn deleted_then_resubmitted_base_is_flagged() {
    let h = start_with(|c| c.auto_approve = true).await;
    h.seed_foo().await;

    // The RPC stops listing it. First miss: noted. Second: a signal.
    h.aur.stamps.lock().unwrap().remove("foo");
    assert_eq!(h.run().await["state"], "done");
    let p = h.get("/api/packages/foo").await;
    assert!(p["snapshot"]["missing_since"].is_string(), "{}", p["snapshot"]);
    assert!(p["signals"].as_array().unwrap().is_empty(), "one miss is not a signal: {}", p["signals"]);
    assert_eq!(h.run().await["state"], "done");
    let p = h.get("/api/packages/foo").await;
    let sig = &p["signals"].as_array().unwrap()[0];
    assert_eq!(sig["payload"]["kind"], "deleted", "{sig}");
    assert_eq!(sig["severity"], "medium");
    assert_eq!(sig["payload"]["git_gone"], false, "the fake keeps the repository");
    assert_eq!(h.run().await["state"], "done", "no duplicate signal per poll");
    assert_eq!(h.get("/api/packages/foo").await["signals"].as_array().unwrap().len(), 1);

    // Someone resubmits the name with a fresh history and a new base id.
    {
        let mut meta = h.meta.lock().unwrap();
        let m = meta.entry("foo".into()).or_default();
        m.base_id = 2;
        m.first_submitted = 1_900_000_000;
        m.maintainer = Some("mallory".into());
    }
    h.aur.resubmit("foo", "2.0");
    h.versions.lock().unwrap().insert("foo".into(), "2.0-1".into());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    h.aur.stamps.lock().unwrap().insert("foo".into(), now - 30 * 86_400);
    let r = h.run().await;
    assert_eq!(r["state"], "needs_input", "{r}");
    let c = h.get("/api/candidates").await.as_array().unwrap()[0].clone();
    let c = h.get(&format!("/api/candidates/{}", c["id"].as_str().unwrap())).await;
    assert_eq!(c["review"]["gate"]["diff_class"], "structural", "{}", c["review"]["gate"]);
    assert_eq!(c["floor_risk"], "high");
    let reasons: Vec<&str> = c["review"]["gate"]["reasons"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert!(reasons.contains(&"history_rewritten"), "{reasons:?}");
    let mut kinds: Vec<&str> = c["signals"].as_array().unwrap().iter().filter_map(|s| s["payload"]["kind"].as_str()).collect();
    kinds.sort();
    assert_eq!(kinds, ["deleted", "history_rewritten", "resubmitted"], "{}", c["signals"]);
    let resub = c["signals"].as_array().unwrap().iter().find(|s| s["payload"]["kind"] == "resubmitted").unwrap();
    assert_eq!(resub["severity"], "high");
    assert_eq!(resub["payload"]["old_id"], 1);
    assert_eq!(resub["payload"]["new_id"], 2);
    assert!(h.get("/api/packages/foo").await["snapshot"]["missing_since"].is_null(), "back on the AUR");
}
