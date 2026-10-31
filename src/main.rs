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
use serde_json::Value;

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
        /// Block until the job finishes and print its stages.
        #[arg(long)]
        wait: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
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
            let v: Value = get(&cli.url, "/api/workers").await?;
            if cli.json {
                return print_json(&v);
            }
            println!("{:<16} {:<8} {:>5} {:>7}  {:<24} STATE", "NAME", "ARCH", "SLOTS", "RUNNING", "LAST SEEN");
            for w in v.as_array().into_iter().flatten() {
                println!(
                    "{:<16} {:<8} {:>5} {:>7}  {:<24} {}",
                    s(&w["name"]),
                    s(&w["arch"]),
                    w["slots"],
                    w["running"].as_array().map_or(0, Vec::len),
                    s(&w["last_seen"]),
                    if w["connected"].as_bool() == Some(true) { "connected" } else { "offline" }
                );
            }
            Ok(())
        }
        Cmd::Jobs { limit } => {
            let v: Value = get(&cli.url, &format!("/api/jobs?limit={limit}")).await?;
            if cli.json {
                return print_json(&v);
            }
            println!("{:<26} {:<8} {:<9} {:<12} {:<24} RESULT", "ID", "KIND", "STATE", "WORKER", "QUEUED");
            for j in v.as_array().into_iter().flatten() {
                println!(
                    "{:<26} {:<8} {:<9} {:<12} {:<24} {}",
                    s(&j["id"]),
                    s(&j["job"]["kind"]),
                    s(&j["state"]),
                    s(&j["worker"]),
                    s(&j["queued_at"]),
                    result_summary(j)
                );
            }
            Ok(())
        }
        Cmd::Job { id, log } => {
            let v: Value = get(&cli.url, &format!("/api/jobs/{id}")).await?;
            if cli.json {
                print_json(&v)?;
            } else {
                print_job(&v);
            }
            if log {
                let lines: Value = get(&cli.url, &format!("/api/jobs/{id}/log")).await?;
                if cli.json {
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
            let body = serde_json::json!({ "job": JobKind::Noop { duration_ms, fail } });
            let created: Value = post(&cli.url, "/api/jobs", &body).await?;
            let id = s(&created["id"]);
            if !wait {
                if cli.json {
                    return print_json(&created);
                }
                println!("{id}");
                return Ok(());
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                let v: Value = get(&cli.url, &format!("/api/jobs/{id}")).await?;
                let state = s(&v["state"]);
                if matches!(state.as_str(), "done" | "failed" | "cancelled") {
                    if cli.json {
                        return print_json(&v);
                    }
                    print_job(&v);
                    return Ok(());
                }
                if std::time::Instant::now() > deadline {
                    anyhow::bail!("job {id} still {state} after 120s");
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

fn print_job(j: &Value) {
    println!("job      {}", s(&j["id"]));
    println!("kind     {}", s(&j["job"]["kind"]));
    println!("state    {}", s(&j["state"]));
    println!("worker   {}", s(&j["worker"]));
    println!("queued   {}", s(&j["queued_at"]));
    println!("started  {}", s(&j["started_at"]));
    println!("finished {}  {}", s(&j["finished_at"]), result_summary(j));
    if let Some(stages) = j["stages"].as_array() {
        for st in stages {
            let dur = match (st["started_at"].as_str(), st["finished_at"].as_str()) {
                (Some(a), Some(b)) => repoman_common::clock::seconds_between(a, b)
                    .map(|d| format!("{d:.3}s"))
                    .unwrap_or_default(),
                _ => "running".to_string(),
            };
            println!("  stage {:<12} {}", s(&st["stage"]), dur);
        }
    }
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

async fn get(base: &str, path: &str) -> Result<Value> {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let resp = reqwest::get(&url).await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("GET {url}: {status} {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("decoding response from {url}"))
}

async fn post(base: &str, path: &str, body: &Value) -> Result<Value> {
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let resp = reqwest::Client::new()
        .post(&url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("POST {url}: {status} {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("decoding response from {url}"))
}
