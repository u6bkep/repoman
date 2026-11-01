use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    /// Address for the HTTP API, UI, and worker WebSocket.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// URL at which workers and build roots reach this coordinator's HTTP
    /// endpoints (images, sources, staging repos, uploads).
    pub public_url: String,
    /// Fast storage: SQLite database, GPG home.
    pub data_dir: PathBuf,
    /// Bulk storage: artifact store, repos, base images, staging, sync
    /// databases, AUR clones. Defaults to `data_dir`.
    #[serde(default)]
    pub bulk_dir: Option<PathBuf>,
    /// Workers allowed to connect, with their shared tokens.
    #[serde(default)]
    pub workers: Vec<WorkerCredential>,
    /// Seconds a job may stay orphaned after its worker disconnects before it
    /// is requeued for another worker.
    #[serde(default = "default_orphan_grace")]
    pub orphan_grace_secs: u64,
    /// Release streams, most conservative first.
    #[serde(default)]
    pub streams: Vec<StreamConfig>,
    #[serde(default = "default_aur_rpc")]
    pub aur_rpc_url: String,
    #[serde(default = "default_aur_git")]
    pub aur_git_url: String,
    /// The AUR website, for package comments shown to the reviewer.
    #[serde(default = "default_aur_web")]
    pub aur_web_url: String,
    /// Target architecture for builds.
    #[serde(default = "default_arch")]
    pub arch: String,
    /// Packages installed into every base image.
    #[serde(default = "default_image_packages")]
    pub image_packages: Vec<String>,
    /// Seconds before a base image is considered stale and rebuilt at the
    /// start of a run.
    #[serde(default = "default_image_max_age")]
    pub image_max_age_secs: u64,
    /// Extra makepkg.conf lines appended inside build roots.
    #[serde(default)]
    pub makepkg_conf_extra: String,
    /// GPG key id or fingerprint used to sign packages and databases.
    /// Unsigned publishing is refused when unset.
    #[serde(default)]
    pub signing_key: Option<String>,
    /// Days a routine or metadata change must have been visible on the AUR
    /// (by the server's LastModified as first observed) before the gate
    /// approves it without an operator. policy.quarantine_days overrides.
    #[serde(default = "default_quarantine_days")]
    pub quarantine_days: u64,
    /// Let the gate approve low-risk routine candidates after quarantine.
    #[serde(default = "default_true")]
    pub auto_approve: bool,
    /// Also approve metadata-only changes; policy.auto_metadata = false
    /// keeps a package manual.
    #[serde(default = "default_true")]
    pub auto_approve_metadata: bool,
    /// Claude API reviewer for candidates the deterministic gate does not
    /// clear on its own.
    #[serde(default)]
    pub reviewer: ReviewerConfig,
    /// Ask GitHub, GitLab, and Codeberg whether the version a routine update
    /// fetches exists as a tag under the baseline's owner.
    #[serde(default = "default_true")]
    pub forge_checks: bool,
    /// Environment variable with a GitHub token (raises the API rate limit).
    #[serde(default = "default_github_token_env")]
    pub github_token_env: String,
    /// Serve upstream package files through /cache (pacman CacheServer) and
    /// point build roots at it.
    #[serde(default = "default_true")]
    pub cache_enabled: bool,
    /// Days a cached file stays after leaving every stream database.
    #[serde(default = "default_cache_grace_days")]
    pub cache_grace_days: u64,
    /// Total cache size above which unreferenced files are evicted early,
    /// least recently served first. Unset means no cap.
    #[serde(default)]
    pub cache_max_bytes: Option<u64>,
    /// Memory assumed for a build with no recorded peaks.
    #[serde(default = "default_build_memory")]
    pub default_build_memory_bytes: u64,
    /// Multiplier on the largest recorded peak when predicting a build's
    /// memory limit.
    #[serde(default = "default_memory_headroom")]
    pub build_memory_headroom: f64,
    /// aur-security (cretezy) lookup service; unset disables the signal.
    #[serde(default = "default_aur_security_url")]
    pub aur_security_url: Option<String>,
    /// Days after an orphan is adopted during which nothing from the new
    /// maintainer is auto-approved.
    #[serde(default = "default_adoption_quarantine_days")]
    pub adoption_quarantine_days: u64,
    /// RSS feed of the aur-requests mailing list; requests against tracked
    /// packages become signals. Unset or empty disables it.
    #[serde(default = "default_aur_requests_url")]
    pub aur_requests_url: Option<String>,
    /// How often to read the feed between runs (it holds 30 items and the
    /// list moves at a few an hour).
    #[serde(default = "default_aur_requests_poll_secs")]
    pub aur_requests_poll_secs: u64,
}

fn default_aur_requests_url() -> Option<String> {
    Some("https://lists.archlinux.org/archives/list/aur-requests@lists.archlinux.org/feed/".into())
}

fn default_aur_requests_poll_secs() -> u64 {
    1800
}

fn default_aur_web() -> String {
    "https://aur.archlinux.org".into()
}

fn default_adoption_quarantine_days() -> u64 {
    30
}

fn default_build_memory() -> u64 {
    4 << 30
}

fn default_memory_headroom() -> f64 {
    1.5
}

fn default_aur_security_url() -> Option<String> {
    Some("https://aur-security.cretezy.com".to_string())
}

fn default_cache_grace_days() -> u64 {
    30
}

fn default_github_token_env() -> String {
    "GITHUB_TOKEN".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Base URL of the Messages API.
    #[serde(default = "default_api_url")]
    pub api_url: String,
    /// Environment variable holding the API key.
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    /// Literal key; prefer the environment variable outside tests.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Model for structural and elevated-risk candidates.
    #[serde(default = "default_review_model")]
    pub model: String,
    /// Model for routine candidates; unset means routine changes are not
    /// sent to the API at all.
    #[serde(default)]
    pub routine_model: Option<String>,
    /// `output_config.effort`.
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default = "default_review_max_tokens")]
    pub max_tokens: u32,
    /// Opt into Anthropic's server-side fallback on a classifier refusal.
    #[serde(default = "default_true")]
    pub fallbacks: bool,
    /// Budget for PKGBUILD text, diff, and scripts in one prompt.
    #[serde(default = "default_review_max_input")]
    pub max_input_chars: usize,
}

impl Default for ReviewerConfig {
    fn default() -> Self {
        ReviewerConfig {
            enabled: false,
            api_url: default_api_url(),
            api_key_env: default_api_key_env(),
            api_key: None,
            model: default_review_model(),
            routine_model: None,
            effort: default_effort(),
            max_tokens: default_review_max_tokens(),
            fallbacks: true,
            max_input_chars: default_review_max_input(),
        }
    }
}

fn default_api_url() -> String {
    "https://api.anthropic.com".to_string()
}

fn default_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}

fn default_review_model() -> String {
    "claude-opus-5-5".to_string()
}

fn default_effort() -> String {
    "high".to_string()
}

fn default_review_max_tokens() -> u32 {
    8000
}

fn default_review_max_input() -> usize {
    240_000
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        CoordinatorConfig {
            listen: default_listen(),
            public_url: "http://127.0.0.1:7800".to_string(),
            data_dir: PathBuf::from("/var/lib/repoman"),
            bulk_dir: None,
            workers: Vec::new(),
            orphan_grace_secs: default_orphan_grace(),
            streams: Vec::new(),
            aur_rpc_url: default_aur_rpc(),
            aur_git_url: default_aur_git(),
            aur_web_url: default_aur_web(),
            arch: default_arch(),
            image_packages: default_image_packages(),
            image_max_age_secs: default_image_max_age(),
            makepkg_conf_extra: String::new(),
            signing_key: None,
            quarantine_days: default_quarantine_days(),
            auto_approve: true,
            auto_approve_metadata: true,
            reviewer: ReviewerConfig::default(),
            forge_checks: true,
            github_token_env: default_github_token_env(),
            cache_enabled: true,
            cache_grace_days: default_cache_grace_days(),
            cache_max_bytes: None,
            default_build_memory_bytes: default_build_memory(),
            build_memory_headroom: default_memory_headroom(),
            aur_security_url: default_aur_security_url(),
            adoption_quarantine_days: default_adoption_quarantine_days(),
            aur_requests_url: default_aur_requests_url(),
            aur_requests_poll_secs: default_aur_requests_poll_secs(),
        }
    }
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            coordinator: "ws://127.0.0.1:7800/ws/worker".to_string(),
            name: String::new(),
            token: String::new(),
            slots: default_slots(),
            scratch_dir: PathBuf::from("/var/lib/repoman-worker"),
            arch: default_host_arch(),
            job_memory_limit: None,
            job_timeout_secs: default_job_timeout(),
            build_uid: default_build_uid(),
            pacstrap_gpgdir: None,
        }
    }
}

impl CoordinatorConfig {
    pub fn bulk_dir(&self) -> PathBuf {
        self.bulk_dir.clone().unwrap_or_else(|| self.data_dir.clone())
    }

    pub fn stream(&self, name: &str) -> Option<&StreamConfig> {
        self.streams.iter().find(|s| s.name == name)
    }

    /// Streams in sort order, most conservative first.
    pub fn streams_sorted(&self) -> Vec<&StreamConfig> {
        let mut v: Vec<&StreamConfig> = self.streams.iter().collect();
        v.sort_by_key(|s| s.sort_order);
        v
    }
}

/// A release stream: one distro branch whose repos a set of client machines
/// track. Each stream gets its own base image and its own published repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    /// Short identifier, also the published repo name, e.g. `manjaro-stable`.
    pub name: String,
    /// Lower sorts first; builds happen in this order.
    #[serde(default)]
    pub sort_order: i32,
    /// Server template with `$repo` and `$arch`, as in pacman.conf.
    pub server: String,
    /// Repos in pacman.conf order.
    pub repos: Vec<String>,
    /// Keyring packages the image needs populated, e.g.
    /// `["archlinux", "manjaro"]` for `pacman-key --populate`.
    #[serde(default)]
    pub keyrings: Vec<String>,
}

fn default_listen() -> String {
    "127.0.0.1:7800".to_string()
}

fn default_orphan_grace() -> u64 {
    120
}

fn default_quarantine_days() -> u64 {
    3
}

fn default_true() -> bool {
    true
}

fn default_aur_rpc() -> String {
    "https://aur.archlinux.org/rpc/v5".to_string()
}

fn default_aur_git() -> String {
    "https://aur.archlinux.org".to_string()
}

fn default_arch() -> String {
    "x86_64".to_string()
}

fn default_image_packages() -> Vec<String> {
    vec!["base".into(), "base-devel".into()]
}

fn default_image_max_age() -> u64 {
    86_400
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
    /// Scratch space: build roots, images, package cache, source cache.
    pub scratch_dir: PathBuf,
    #[serde(default = "default_host_arch")]
    pub arch: String,
    /// Per-job memory limit in bytes, enforced through the job's cgroup when
    /// the worker runs as root. Unset means no limit.
    #[serde(default)]
    pub job_memory_limit: Option<u64>,
    /// Seconds a build job may run before it is killed.
    #[serde(default = "default_job_timeout")]
    pub job_timeout_secs: u64,
    /// Unprivileged uid and gid used for makepkg inside build roots.
    #[serde(default = "default_build_uid")]
    pub build_uid: u32,
    /// Host-side pacman keyring directory used by pacstrap to verify stream
    /// packages while building images. Defaults to the host's own keyring.
    #[serde(default)]
    pub pacstrap_gpgdir: Option<PathBuf>,
}

impl WorkerConfig {
    /// HTTP base derived from the WebSocket URL, for uploads and downloads.
    pub fn coordinator_http(&self) -> String {
        let s = self
            .coordinator
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1);
        match s.find("/ws/") {
            Some(i) => s[..i].to_string(),
            None => s.trim_end_matches('/').to_string(),
        }
    }
}

fn default_slots() -> u32 {
    2
}

fn default_host_arch() -> String {
    std::env::consts::ARCH.to_string()
}

fn default_job_timeout() -> u64 {
    4 * 3600
}

fn default_build_uid() -> u32 {
    1500
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
    fn worker_defaults_and_http_base() {
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
        assert_eq!(w.coordinator_http(), "http://localhost:7800");
        assert_eq!(w.build_uid, 1500);
    }

    #[test]
    fn coordinator_streams_sorted() {
        let c: CoordinatorConfig = toml::from_str(
            r#"
            public_url = "http://c:7800"
            data_dir = "/tmp/d"
            [[streams]]
            name = "manjaro-unstable"
            sort_order = 1
            server = "https://m/manjaro/unstable/$repo/$arch"
            repos = ["core", "extra"]
            [[streams]]
            name = "manjaro-stable"
            sort_order = 0
            server = "https://m/manjaro/stable/$repo/$arch"
            repos = ["core", "extra", "multilib"]
            keyrings = ["archlinux", "manjaro"]
            "#,
        )
        .unwrap();
        let names: Vec<&str> = c.streams_sorted().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["manjaro-stable", "manjaro-unstable"]);
        assert_eq!(c.bulk_dir(), PathBuf::from("/tmp/d"));
        assert_eq!(c.image_packages, ["base", "base-devel"]);
    }
}
