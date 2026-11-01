//! `repoman`: one binary, several roles.
//!
//! - `repoman coordinator --config FILE`
//! - `repoman worker --config FILE`
//! - CLI verbs that talk to a coordinator's HTTP API.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use repoman_common::config;
use repoman_common::proto::JobKind;
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "repoman", version, about = "AUR build coordinator, worker, and CLI")]
struct Cli {
    /// Coordinator API base URL for CLI verbs. Also read from REPOMAN_URL.
    #[arg(long, global = true, env = "REPOMAN_URL", default_value = "http://127.0.0.1:7800")]
    url: String,
    /// Print raw JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the coordinator role.
    Coordinator {
        #[arg(long)]
        config: PathBuf,
    },
    /// Run the worker role.
    Worker {
        #[arg(long)]
        config: PathBuf,
    },
    /// List known workers and their connection state.
    Workers,
    /// List recent jobs.
    Jobs {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show one job, with stages and optionally its log.
    Job {
        id: String,
        #[arg(long)]
        log: bool,
    },
    /// Submit a no-op job to exercise the pipeline.
    Noop {
        #[arg(long, default_value_t = 500)]
        duration_ms: u64,
        #[arg(long)]
        fail: bool,
        #[arg(long)]
        wait: bool,
    },
    /// Add a package base to the wanted set.
    Add {
        pkgbase: Vec<String>,
        /// Stream to want it on; repeatable. Default: all streams.
        #[arg(long = "stream", short = 's')]
        streams: Vec<String>,
        /// Provenance class: upstream-binary, upstream-source, vcs, repackaged, authored.
        #[arg(long)]
        class: Option<String>,
        /// Git URL of a repository with the PKGBUILD at its root, instead of
        /// the AUR. Append #branch=<name> to follow a branch other than the
        /// remote's default.
        #[arg(long)]
        source: Option<String>,
    },
    /// Remove a package base and its memberships, or with --stream only
    /// its membership on that stream.
    Rm {
        pkgbase: String,
        #[arg(long = "stream", short = 's')]
        stream: Option<String>,
    },
    /// List packages.
    Packages,
    /// Show one package.
    Package { pkgbase: String },
    /// Set a package's policy JSON (replaces the whole object).
    Policy { pkgbase: String, policy: String },
    /// Start or resume an update run.
    Update {
        /// Block until the run finishes or needs input.
        #[arg(long)]
        wait: bool,
    },
    /// List runs.
    Runs,
    /// Show one run.
    Run { id: String },
    /// List candidates awaiting a decision (or --state).
    Candidates {
        #[arg(long)]
        state: Option<String>,
    },
    /// Show a candidate's review details.
    Candidate { id: String },
    /// Approve a candidate.
    Approve {
        id: String,
        #[arg(long)]
        class: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Reject a candidate until its package's head commit moves.
    Reject {
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Defer a candidate.
    Defer { id: String },
    /// List base images.
    Images,
    /// Generate a signing key on the coordinator.
    Keygen {
        #[arg(long, default_value = "repoman <repoman@localhost>")]
        uid: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let api = Api {
        base: cli.url.trim_end_matches('/').to_string(),
        json: cli.json,
    };
    match cli.cmd {
        Cmd::Coordinator { config } => {
            let cfg = config::load(&config)?;
            repoman_coordinator::run(cfg).await
        }
        Cmd::Worker { config } => {
            let cfg = config::load(&config)?;
            repoman_worker::run(cfg).await
        }
        Cmd::Workers => {
            let v = api.get("/api/workers").await?;
            api.table(&v, &["name", "arch", "slots", "running#", "capabilities", "last_seen", "connected"])
        }
        Cmd::Jobs { limit } => {
            let v = api.get(&format!("/api/jobs?limit={limit}")).await?;
            api.table(&v, &["id", "job.kind", "pkgbase", "stream", "state", "worker", "queued_at", "error"])
        }
        Cmd::Job { id, log } => {
            let v = api.get(&format!("/api/jobs/{id}")).await?;
            if api.json {
                print_json(&v)?;
            } else {
                print_job(&v);
            }
            if log {
                let lines = api.get(&format!("/api/jobs/{id}/log")).await?;
                if api.json {
                    print_json(&lines)?;
                } else {
                    for l in lines.as_array().into_iter().flatten() {
                        println!("{} [{}] {}", s(&l["ts"]), s(&l["fd"]), s(&l["line"]));
                    }
                }
            }
            Ok(())
        }
        Cmd::Noop { duration_ms, fail, wait } => {
            let created = api.post("/api/jobs", &json!({ "job": JobKind::Noop { duration_ms, fail } })).await?;
            let id = s(&created["id"]);
            if !wait {
                return api.emit(&created, || println!("{id}"));
            }
            let v = api
                .poll(&format!("/api/jobs/{id}"), |v| matches!(s(&v["state"]).as_str(), "done" | "failed" | "cancelled"), 120)
                .await?;
            api.emit(&v, || print_job(&v))
        }
        Cmd::Add { pkgbase, streams, class, source } => {
            for pb in pkgbase {
                let v = api
                    .post("/api/packages", &json!({"pkgbase": pb, "streams": streams, "class": class, "source": source}))
                    .await?;
                api.emit(&v, || {
                    let st: Vec<String> = v["streams"].as_array().into_iter().flatten().map(|m| s(&m["stream"])).collect();
                    println!("{pb}: streams {}", st.join(", "));
                })?;
            }
            Ok(())
        }
        Cmd::Rm { pkgbase, stream } => {
            match stream {
                Some(s) => {
                    api.delete(&format!("/api/packages/{pkgbase}/streams/{s}")).await?;
                    println!("removed {pkgbase} from {s}");
                }
                None => {
                    api.delete(&format!("/api/packages/{pkgbase}")).await?;
                    println!("removed {pkgbase}");
                }
            }
            Ok(())
        }
        Cmd::Packages => {
            let v = api.get("/api/packages").await?;
            if api.json {
                return print_json(&v);
            }
            println!("{:<32} {:<8} {:<16} {:<12} {:<24} BASELINE", "PKGBASE", "KIND", "CLASS", "VERSION", "STREAMS");
            for p in v.as_array().into_iter().flatten() {
                let st: Vec<String> = p["streams"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|m| format!("{}{}", s(&m["stream"]), if m["required"] == true && m["wanted"] != true { "*" } else { "" }))
                    .collect();
                println!(
                    "{:<32} {:<8} {:<16} {:<12} {:<24} {}",
                    s(&p["pkgbase"]),
                    s(&p["kind"]),
                    s(&p["provenance_class"]),
                    s(&p["snapshot"]["version"]),
                    st.join(","),
                    p["baseline"]["aur_commit"].as_str().map(|c| &c[..12.min(c.len())]).unwrap_or("-")
                );
            }
            Ok(())
        }
        Cmd::Package { pkgbase } => {
            let v = api.get(&format!("/api/packages/{pkgbase}")).await?;
            print_json(&v)
        }
        Cmd::Policy { pkgbase, policy } => {
            let p: Value = serde_json::from_str(&policy).context("policy must be JSON")?;
            api.post_status(&format!("/api/packages/{pkgbase}/policy"), &json!({"policy": p})).await?;
            println!("policy set");
            Ok(())
        }
        Cmd::Update { wait } => {
            let v = api.post("/api/runs", &json!({})).await?;
            let id = s(&v["id"]);
            if !wait {
                return api.emit(&v, || println!("run {id}"));
            }
            let v = api
                .poll(
                    &format!("/api/runs/{id}"),
                    |v| v["finished_at"].is_string() || s(&v["state"]) == "needs_input",
                    6 * 3600,
                )
                .await?;
            api.emit(&v, || print_run(&v))
        }
        Cmd::Runs => {
            let v = api.get("/api/runs").await?;
            api.table(&v, &["id", "state", "step", "started_at", "finished_at"])
        }
        Cmd::Run { id } => {
            let v = api.get(&format!("/api/runs/{id}")).await?;
            api.emit(&v, || print_run(&v))
        }
        Cmd::Candidates { state } => {
            let path = match state {
                Some(st) => format!("/api/candidates?state={st}"),
                None => "/api/candidates".to_string(),
            };
            let v = api.get(&path).await?;
            if api.json {
                return print_json(&v);
            }
            println!(
                "{:<26} {:<28} {:<19} {:<13} {:<16} {:<11} {:<7} SUMMARY",
                "ID", "PKGBASE", "KIND", "STATE", "CLASS", "DIFF", "RISK"
            );
            for c in v.as_array().into_iter().flatten() {
                println!(
                    "{:<26} {:<28} {:<19} {:<13} {:<16} {:<11} {:<7} {}",
                    s(&c["id"]),
                    s(&c["pkgbase"]),
                    s(&c["kind"]),
                    s(&c["state"]),
                    s(&c["class"]),
                    s(&c["review"]["gate"]["diff_class"]),
                    s(&c["floor_risk"]),
                    candidate_summary(c)
                );
            }
            Ok(())
        }
        Cmd::Candidate { id } => {
            let v = api.get(&format!("/api/candidates/{id}")).await?;
            api.emit(&v, || print_candidate(&v))
        }
        Cmd::Approve { id, class, note } => {
            let v = api
                .post(&format!("/api/candidates/{id}/decide"), &json!({"decision": "approve", "class": class, "note": note}))
                .await?;
            api.emit(&v, || println!("{} {} -> {}", s(&v["pkgbase"]), s(&v["kind"]), s(&v["state"])))
        }
        Cmd::Reject { id, note } => {
            let v = api
                .post(&format!("/api/candidates/{id}/decide"), &json!({"decision": "reject", "note": note}))
                .await?;
            api.emit(&v, || println!("{} -> {}", s(&v["pkgbase"]), s(&v["state"])))
        }
        Cmd::Defer { id } => {
            let v = api.post(&format!("/api/candidates/{id}/decide"), &json!({"decision": "defer"})).await?;
            api.emit(&v, || println!("{} -> {}", s(&v["pkgbase"]), s(&v["state"])))
        }
        Cmd::Images => {
            let v = api.get("/api/images").await?;
            api.table(&v, &["stream", "sha256", "size", "created_at", "state"])
        }
        Cmd::Keygen { uid } => {
            let v = api.post("/api/signing-key", &json!({"uid": uid})).await?;
            api.emit(&v, || println!("fingerprint {}\n{}", s(&v["fingerprint"]), s(&v["note"])))
        }
    }
}

struct Api {
    base: String,
    json: bool,
}

impl Api {
    async fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = reqwest::get(&url).await.with_context(|| format!("GET {url}"))?;
        decode(resp, &format!("GET {url}")).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base, path);
        let resp = reqwest::Client::new().post(&url).json(body).send().await.with_context(|| format!("POST {url}"))?;
        decode(resp, &format!("POST {url}")).await
    }

    async fn post_status(&self, path: &str, body: &Value) -> Result<()> {
        let url = format!("{}{}", self.base, path);
        let resp = reqwest::Client::new().post(&url).json(body).send().await.with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("POST {url}: {status} {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let url = format!("{}{}", self.base, path);
        let resp = reqwest::Client::new().delete(&url).send().await.with_context(|| format!("DELETE {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("DELETE {url}: {status} {}", resp.text().await.unwrap_or_default());
        }
        Ok(())
    }

    async fn poll(&self, path: &str, done: impl Fn(&Value) -> bool, timeout_secs: u64) -> Result<Value> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            let v = self.get(path).await?;
            if done(&v) {
                return Ok(v);
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!("timed out waiting on {path}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    fn emit(&self, v: &Value, human: impl FnOnce()) -> Result<()> {
        if self.json {
            print_json(v)
        } else {
            human();
            Ok(())
        }
    }

    /// Print an array of objects as a table. A column spec `a.b` reads a
    /// nested field; `x#` prints the length of array `x`.
    fn table(&self, v: &Value, cols: &[&str]) -> Result<()> {
        if self.json {
            return print_json(v);
        }
        let rows: Vec<Vec<String>> = v
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| cols.iter().map(|c| cell(row, c)).collect())
            .collect();
        let mut widths: Vec<usize> = cols.iter().map(|c| c.trim_end_matches('#').len()).collect();
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count().min(40));
            }
        }
        let header: Vec<String> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<w$}", c.trim_end_matches('#').rsplit('.').next().unwrap().to_uppercase(), w = widths[i]))
            .collect();
        println!("{}", header.join("  ").trim_end());
        for r in rows {
            let line: Vec<String> = r.iter().enumerate().map(|(i, c)| format!("{:<w$}", truncate(c, 40), w = widths[i])).collect();
            println!("{}", line.join("  ").trim_end());
        }
        Ok(())
    }
}

async fn decode(resp: reqwest::Response, what: &str) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("{what}: {status} {text}");
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).with_context(|| format!("decoding response from {what}"))
}

fn cell(row: &Value, spec: &str) -> String {
    if let Some(key) = spec.strip_suffix('#') {
        return row[key].as_array().map_or(0, Vec::len).to_string();
    }
    let mut v = row;
    for part in spec.split('.') {
        v = &v[part];
    }
    match v {
        Value::Array(a) => a.iter().map(s).collect::<Vec<_>>().join(","),
        Value::Bool(true) if spec == "connected" => "connected".into(),
        Value::Bool(false) if spec == "connected" => "offline".into(),
        other => s(other),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}

fn print_job(j: &Value) {
    println!("job      {}", s(&j["id"]));
    println!("kind     {}  {} {}", s(&j["job"]["kind"]), s(&j["pkgbase"]), s(&j["stream"]));
    println!("state    {}", s(&j["state"]));
    println!("worker   {}", s(&j["worker"]));
    println!("queued   {}", s(&j["queued_at"]));
    println!("started  {}", s(&j["started_at"]));
    println!("finished {}  {}", s(&j["finished_at"]), result_summary(j));
    if let Some(p) = j["peaks"].as_object()
        && !p.is_empty() {
            println!("peaks    {}", serde_json::to_string(&j["peaks"]).unwrap_or_default());
        }
    if let Some(stages) = j["stages"].as_array() {
        for st in stages {
            let dur = match (st["started_at"].as_str(), st["finished_at"].as_str()) {
                (Some(a), Some(b)) => repoman_common::clock::seconds_between(a, b).map(|d| format!("{d:.3}s")).unwrap_or_default(),
                _ => "running".to_string(),
            };
            println!("  stage {:<12} {}", s(&st["stage"]), dur);
        }
    }
}

fn print_run(r: &Value) {
    println!("run      {}", s(&r["id"]));
    println!("state    {}  step {}", s(&r["state"]), s(&r["step"]));
    println!("started  {}", s(&r["started_at"]));
    println!("finished {}", s(&r["finished_at"]));
    if let Some(sum) = r["summary"].as_object()
        && !sum.is_empty() {
            println!("summary  {}", serde_json::to_string(&r["summary"]).unwrap_or_default());
        }
    if let Some(cands) = r["candidates"].as_array()
        && !cands.is_empty() {
            println!("candidates:");
            for c in cands {
                println!("  {:<26} {:<28} {:<20} {:<12} {}", s(&c["id"]), s(&c["pkgbase"]), s(&c["kind"]), s(&c["state"]), candidate_summary(c));
            }
        }
    if let Some(jobs) = r["jobs"].as_array()
        && !jobs.is_empty() {
            println!("jobs:");
            for j in jobs {
                println!("  {:<26} {:<8} {:<28} {:<18} {:<8} {}", s(&j["id"]), s(&j["job"]["kind"]), s(&j["pkgbase"]), s(&j["stream"]), s(&j["state"]), s(&j["error"]));
            }
        }
}

fn print_candidate(c: &Value) {
    let r = &c["review"];
    println!(
        "{} {} {} {}  (decided by {})",
        s(&c["pkgbase"]),
        s(&c["kind"]),
        s(&c["state"]),
        s(&c["id"]),
        s(&c["decided_by"])
    );
    if let (Some(a), Some(b)) = (r["version_from"].as_str(), r["version_to"].as_str()) {
        println!("version   {a} -> {b}");
    }
    if let Some(to) = c["aur_commit"].as_str() {
        println!("commit    {} -> {}", r["from"].as_str().map(|x| &x[..x.len().min(10)]).unwrap_or("-"), &to[..to.len().min(10)]);
    }
    println!(
        "class     {}   diff {}   floor risk {}   llm risk {}",
        s(&c["class"]),
        s(&r["gate"]["diff_class"]),
        s(&c["floor_risk"]),
        s(&c["llm_risk"])
    );
    if let Some(n) = c["note"].as_str() {
        println!("note      {n}");
    }
    if let Some(e) = r["gate_error"].as_str() {
        println!("gate      error: {e}");
    }
    if let Some(reasons) = r["gate"]["reasons"].as_array()
        && !reasons.is_empty()
    {
        println!("reasons:");
        for x in reasons {
            println!("  [{:<6}] {:<28} {}", s(&x["risk"]), s(&x["kind"]), s(&x["detail"]));
        }
    }
    if let Some(findings) = r["gate"]["findings"].as_array()
        && !findings.is_empty()
    {
        println!("findings:");
        for f in findings {
            let where_ = match f["function"].as_str() {
                Some(func) => format!("{}:{} {}()", s(&f["file"]), s(&f["line"]), func),
                None => format!("{}:{}", s(&f["file"]), s(&f["line"])),
            };
            let note = f["note"].as_str().map(|n| format!("  ({n})")).unwrap_or_default();
            println!("  [{:<6}] {:<22} {:<34} {}{}", s(&f["severity"]), s(&f["rule"]), where_, s(&f["excerpt"]), note);
        }
    }
    let llm = &r["llm"];
    if !llm.is_null() {
        let served = llm["served_by"].as_str().filter(|m| Some(*m) != llm["model"].as_str()).map(|m| format!(" (served by {m})")).unwrap_or_default();
        println!(
            "llm       {}{}  ${:.4}  in {} out {}",
            s(&llm["model"]),
            served,
            llm["cost_usd"].as_f64().unwrap_or(0.0),
            s(&llm["usage"]["input_tokens"]),
            s(&llm["usage"]["output_tokens"])
        );
        if let Some(e) = llm["error"].as_str() {
            println!("          no verdict: {e}");
        }
        if let Some(cat) = llm["refused"].as_str() {
            println!("          refused ({cat}); no verdict");
        }
        let v = &llm["verdict"];
        if !v.is_null() {
            println!("          risk {}: {}", s(&v["risk"]), s(&v["summary"]));
            for f in v["findings"].as_array().into_iter().flatten() {
                println!("  [{:<6}] {:<22} {}", s(&f["severity"]), s(&f["kind"]), s(&f["evidence"]));
            }
            for q in v["questions_for_human"].as_array().into_iter().flatten() {
                println!("  ? {}", s(q));
            }
        }
    }
    if let Some(commits) = r["commits"].as_array()
        && !commits.is_empty()
    {
        println!("commits:");
        for k in commits {
            let h = k["hash"].as_str().unwrap_or("");
            println!("  {} {} {:<24} {}", &h[..h.len().min(10)], s(&k["date"]), truncate(&s(&k["author"]), 24), s(&k["subject"]));
        }
    }
    if let Some(up) = r["upstream_rev"].as_str() {
        println!("upstream  {up}");
    }
}

fn candidate_summary(c: &Value) -> String {
    let r = &c["review"];
    let mut parts = Vec::new();
    if let (Some(a), Some(b)) = (r["version_from"].as_str(), r["version_to"].as_str()) {
        parts.push(format!("{a} -> {b}"));
    }
    if let Some(reasons) = r["gate"]["reasons"].as_array() {
        let mut kinds: Vec<&str> = reasons
            .iter()
            .filter(|x| x["risk"].as_str() != Some("low"))
            .filter_map(|x| x["kind"].as_str())
            .collect();
        kinds.dedup();
        if !kinds.is_empty() {
            parts.push(kinds.join(" "));
        }
    } else if let Some(ch) = r["changes"].as_array() {
        let keys: Vec<String> = ch.iter().map(|x| format!("{}.{}", s(&x["section"]), s(&x["key"]))).collect();
        parts.push(format!("changed: {}", keys.join(" ")));
    }
    if let Some(findings) = r["gate"]["findings"].as_array() {
        let n = findings.iter().filter(|f| f["severity"].as_str() != Some("low")).count();
        if n > 0 {
            parts.push(format!("{n} lint finding{}", if n == 1 { "" } else { "s" }));
        }
    }
    if let Some(risk) = c["llm_risk"].as_str() {
        parts.push(format!("llm {risk}"));
    } else if r["llm"]["error"].is_string() || r["llm"]["refused"].is_string() {
        parts.push("llm: no verdict".to_string());
    }
    if let Some(n) = r["note"].as_str() {
        parts.push(n.to_string());
    }
    if let Some(n) = c["note"].as_str() {
        parts.push(n.to_string());
    }
    truncate(&parts.join("; "), 100)
}

fn result_summary(j: &Value) -> String {
    match (j["exit_code"].as_i64(), j["error"].as_str()) {
        (None, None) => String::new(),
        (Some(0), None) => "exit 0".to_string(),
        (code, err) => format!("exit {} {}", code.map_or("?".to_string(), |c| c.to_string()), err.unwrap_or("")),
    }
}

/// Render a JSON scalar for a table cell; null becomes "-".
fn s(v: &Value) -> String {
    match v {
        Value::Null => "-".to_string(),
        Value::String(x) => x.clone(),
        other => other.to_string(),
    }
}

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}
