use anyhow::{Context, Result};
use confique::Config as ConfigTrait;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Repoman configuration
#[derive(Debug, Clone, confique::Config, Serialize)]
pub struct Config {
    /// Repository settings for built packages
    #[config(nested)]
    pub repo: RepoConfig,

    /// Cache and storage paths
    #[config(nested)]
    pub cache: CacheConfig,

    /// Container build settings
    #[config(nested)]
    pub container: ContainerConfig,

    /// Web server settings
    #[config(nested)]
    pub server: ServerConfig,
}

/// Repository configuration
#[derive(Debug, Clone, confique::Config, Serialize)]
pub struct RepoConfig {
    /// Path to repository directory where built packages are stored
    #[config(default = "/srv/repo/aur", env = "REPOMAN_REPO_PATH")]
    pub path: PathBuf,

    /// Repository name (used in pacman.conf)
    #[config(default = "aur-local", env = "REPOMAN_REPO_NAME")]
    pub name: String,

    /// Number of old package versions to keep
    #[config(default = 2, env = "REPOMAN_REPO_KEEP_VERSIONS")]
    pub keep_versions: u32,
}

/// Cache and storage paths
///
/// All paths default to XDG-compliant locations under ~/.cache/repoman
/// and ~/.local/share/repoman if not specified.
#[derive(Debug, Clone, confique::Config, Serialize)]
pub struct CacheConfig {
    /// Path to AUR git clones
    #[config(env = "REPOMAN_CACHE_AUR_CLONES")]
    pub aur_clones: Option<PathBuf>,

    /// Path to shared pacman package cache
    #[config(env = "REPOMAN_CACHE_PACMAN")]
    pub pacman_cache: Option<PathBuf>,

    /// Path to build logs
    #[config(env = "REPOMAN_CACHE_LOGS")]
    pub logs: Option<PathBuf>,

    /// Path to downloaded sources (SRCDEST) - per-package subdirectories
    #[config(env = "REPOMAN_CACHE_SOURCES")]
    pub sources: Option<PathBuf>,
}

/// Container build settings
#[derive(Debug, Clone, confique::Config, Serialize)]
pub struct ContainerConfig {
    /// Base image name for builds
    #[config(default = "repoman-builder", env = "REPOMAN_CONTAINER_IMAGE")]
    pub image: String,

    /// Use tmpfs (ramdisk) for build directory - faster but uses RAM
    #[config(default = false, env = "REPOMAN_CONTAINER_TMPFS_BUILD")]
    pub tmpfs_build: bool,

    /// Size limit for tmpfs (e.g., "4G", "8G") - defaults to no limit
    #[config(env = "REPOMAN_CONTAINER_TMPFS_SIZE")]
    pub tmpfs_size: Option<String>,
}

/// Web server settings
#[derive(Debug, Clone, confique::Config, Serialize)]
pub struct ServerConfig {
    /// API server bind address (host:port)
    #[config(default = "127.0.0.1:8080", env = "REPOMAN_SERVER_LISTEN")]
    pub listen: String,
}

// Computed cache paths with defaults

fn cache_base() -> PathBuf {
    directories::ProjectDirs::from("", "", "repoman")
        .map(|d| d.cache_dir().to_path_buf())
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache/repoman")))
        .unwrap_or_else(|| PathBuf::from("/tmp/repoman/cache"))
}

fn data_base() -> PathBuf {
    directories::ProjectDirs::from("", "", "repoman")
        .map(|d| d.data_dir().to_path_buf())
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share/repoman")))
        .unwrap_or_else(|| PathBuf::from("/tmp/repoman/data"))
}

/// Resolved cache config with all paths filled in
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedCacheConfig {
    pub aur_clones: PathBuf,
    pub pacman_cache: PathBuf,
    pub logs: PathBuf,
    pub sources: PathBuf,
}

impl CacheConfig {
    /// Resolve optional paths to concrete values using XDG defaults
    pub fn resolve(&self) -> ResolvedCacheConfig {
        let cache = cache_base();
        let data = data_base();

        ResolvedCacheConfig {
            aur_clones: self.aur_clones.clone().unwrap_or_else(|| cache.join("aur")),
            pacman_cache: self
                .pacman_cache
                .clone()
                .unwrap_or_else(|| cache.join("pacman")),
            logs: self.logs.clone().unwrap_or_else(|| data.join("logs")),
            sources: self
                .sources
                .clone()
                .unwrap_or_else(|| cache.join("sources")),
        }
    }
}

impl Config {
    /// Load configuration from file, falling back to defaults.
    ///
    /// Configuration is loaded in this order (later sources override earlier):
    /// 1. Default values
    /// 2. Config file (~/.config/repoman/config.toml or specified path)
    /// 3. Environment variables (prefixed with REPOMAN_)
    ///
    /// Environment variable examples:
    /// - REPOMAN_REPO_PATH=/srv/repo/custom
    /// - REPOMAN_REPO_NAME=my-aur
    /// - REPOMAN_SERVER_LISTEN=0.0.0.0:8088
    /// - REPOMAN_CONTAINER_TMPFS_BUILD=true
    /// - REPOMAN_CONTAINER_IMAGE=custom-builder
    /// - REPOMAN_CACHE_AUR_CLONES=/custom/path/aur
    /// - REPOMAN_CACHE_LOGS=/var/log/repoman
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| {
                directories::ProjectDirs::from("", "", "repoman")
                    .map(|dirs| dirs.config_dir().join("config.toml"))
            })
            .or_else(|| dirs::home_dir().map(|h| h.join(".config/repoman/config.toml")));

        let mut builder = <Config as ConfigTrait>::builder();

        if let Some(ref path) = config_path
            && path.exists()
        {
            builder = builder.file(path);
        }

        // Add environment variable support with REPOMAN_ prefix
        builder = builder.env();

        let config = builder.load().with_context(|| {
            if let Some(path) = config_path {
                format!("Failed to load config from {}", path.display())
            } else {
                "Failed to load config".to_string()
            }
        })?;

        // Validate repo name: only [a-zA-Z0-9_-] allowed
        if config.repo.name.is_empty()
            || !config
                .repo
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            anyhow::bail!(
                "Invalid repo name '{}': only alphanumeric characters, underscores, and hyphens are allowed",
                config.repo.name
            );
        }

        // Validate tmpfs_size if set: must match \d+[KMGkmg]?[Bb]?
        if let Some(ref size) = config.container.tmpfs_size {
            let valid = {
                let mut chars = size.chars().peekable();
                // Must start with one or more digits
                let has_digits = chars.by_ref().take_while(|c| c.is_ascii_digit()).count() > 0;
                // Optionally followed by a unit letter (K, M, G, case-insensitive)
                let has_unit = chars
                    .next_if(|c| matches!(c, 'K' | 'M' | 'G' | 'k' | 'm' | 'g'))
                    .is_some();
                // Optionally followed by 'B' or 'b'
                let _ = has_unit; // suppress unused warning
                chars.next_if(|c| matches!(c, 'B' | 'b'));
                // Must be fully consumed
                has_digits && chars.next().is_none()
            };
            if !valid {
                anyhow::bail!(
                    "Invalid tmpfs_size '{}': expected a number followed by an optional unit (K, M, G) and optional 'B', e.g. '4G', '512M', '1024'",
                    size
                );
            }
        }

        Ok(config)
    }

    /// Get resolved cache paths (with defaults filled in)
    pub fn resolved_cache(&self) -> ResolvedCacheConfig {
        self.cache.resolve()
    }

    /// Ensure all required directories exist
    pub fn ensure_dirs(&self) -> Result<()> {
        let cache = self.resolved_cache();

        std::fs::create_dir_all(&self.repo.path)
            .with_context(|| format!("Failed to create repo dir: {}", self.repo.path.display()))?;
        std::fs::create_dir_all(&cache.aur_clones).with_context(|| {
            format!(
                "Failed to create aur_clones dir: {}",
                cache.aur_clones.display()
            )
        })?;
        std::fs::create_dir_all(&cache.pacman_cache).with_context(|| {
            format!(
                "Failed to create pacman_cache dir: {}",
                cache.pacman_cache.display()
            )
        })?;
        std::fs::create_dir_all(&cache.logs)
            .with_context(|| format!("Failed to create logs dir: {}", cache.logs.display()))?;
        std::fs::create_dir_all(&cache.sources).with_context(|| {
            format!("Failed to create sources dir: {}", cache.sources.display())
        })?;
        Ok(())
    }

    /// Generate a TOML template with all options documented
    pub fn template() -> String {
        confique::toml::template::<Config>(confique::toml::FormatOptions::default())
    }

    /// Serialize current config to TOML (shows effective values)
    pub fn to_toml(&self) -> Result<String> {
        // Create a version with resolved paths for display
        #[derive(Serialize)]
        struct DisplayConfig<'a> {
            repo: &'a RepoConfig,
            cache: ResolvedCacheConfig,
            container: &'a ContainerConfig,
            server: &'a ServerConfig,
        }

        let display = DisplayConfig {
            repo: &self.repo,
            cache: self.resolved_cache(),
            container: &self.container,
            server: &self.server,
        };

        toml::to_string_pretty(&display).context("Failed to serialize config")
    }
}

/// Get the default config file path
pub fn get_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "repoman")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .or_else(|| dirs::home_dir().map(|h| h.join(".config/repoman/config.toml")))
}

/// Update the repository configuration in the config file
///
/// Uses toml_edit to preserve formatting and comments.
pub fn update_config_repo(repo_path: &Path, repo_name: &str) -> Result<PathBuf> {
    let config_path =
        get_config_path().ok_or_else(|| anyhow::anyhow!("Could not determine config path"))?;

    // Read existing config or create empty
    let config_str = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config: {}", config_path.display()))?
    } else {
        String::new()
    };

    // Parse as TOML document
    let mut doc: toml_edit::DocumentMut = config_str
        .parse()
        .context("Failed to parse config as TOML")?;

    // Ensure [repo] section exists
    if !doc.contains_key("repo") {
        doc["repo"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    // Update repo settings
    doc["repo"]["path"] = toml_edit::value(repo_path.to_string_lossy().to_string());
    doc["repo"]["name"] = toml_edit::value(repo_name);

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    // Write back
    std::fs::write(&config_path, doc.to_string())
        .with_context(|| format!("Failed to write config: {}", config_path.display()))?;

    Ok(config_path)
}
