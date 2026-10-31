use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    /// Address for the HTTP API, UI, and worker WebSocket.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Directory for the SQLite database. Should live on fast storage.
    pub data_dir: PathBuf,
    /// Workers allowed to connect, with their shared tokens.
    #[serde(default)]
    pub workers: Vec<WorkerCredential>,
    /// Seconds a job may stay orphaned after its worker disconnects before it
    /// is requeued for another worker.
    #[serde(default = "default_orphan_grace")]
    pub orphan_grace_secs: u64,
}

fn default_listen() -> String {
    "127.0.0.1:7800".to_string()
}

fn default_orphan_grace() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerCredential {
    pub name: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// WebSocket URL of the coordinator, e.g. `ws://nas:7800/ws/worker`.
    pub coordinator: String,
    pub name: String,
    pub token: String,
    /// Concurrent jobs this worker will run.
    #[serde(default = "default_slots")]
    pub slots: u32,
    /// Scratch space for build roots and uploads.
    pub scratch_dir: PathBuf,
    #[serde(default = "default_arch")]
    pub arch: String,
}

fn default_slots() -> u32 {
    2
}

fn default_arch() -> String {
    std::env::consts::ARCH.to_string()
}

pub fn load<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_defaults() {
        let w: WorkerConfig = toml::from_str(
            r#"
            coordinator = "ws://localhost:7800/ws/worker"
            name = "desktop"
            token = "secret"
            scratch_dir = "/tmp/x"
            "#,
        )
        .unwrap();
        assert_eq!(w.slots, 2);
        assert_eq!(w.arch, std::env::consts::ARCH);
    }
}
