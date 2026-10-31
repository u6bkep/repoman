//! Release streams: fetch each stream's sync databases from its mirror and
//! keep a parsed index per stream. Databases are cached on disk under
//! `bulk/streams/<stream>/<repo>.db` with their `Last-Modified` header so a
//! refresh is a conditional GET.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use repoman_alpm::pacman_conf::{PacmanConf, Repo, expand_server};
use repoman_alpm::syncdb::{SyncIndex, read_db};
use repoman_common::clock;
use repoman_common::config::{CoordinatorConfig, StreamConfig};
use sha2::{Digest, Sha256};

pub struct StreamIndex {
    pub name: String,
    pub index: SyncIndex,
    pub fetched_at: String,
    /// sha256 of each repo's database file, in repo order.
    pub db_hashes: Vec<(String, String)>,
}

impl StreamIndex {
    /// Hash over the stream's database hashes: changes whenever any repo
    /// changed. Used to decide whether a base image is stale.
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for (repo, hash) in &self.db_hashes {
            h.update(repo.as_bytes());
            h.update(b"=");
            h.update(hash.as_bytes());
            h.update(b"\n");
        }
        hex::encode(h.finalize())
    }
}

pub struct Streams {
    config: Arc<CoordinatorConfig>,
    dir: PathBuf,
    http: reqwest::Client,
    indexes: RwLock<HashMap<String, Arc<StreamIndex>>>,
}

impl Streams {
    pub fn new(config: Arc<CoordinatorConfig>) -> Result<Self> {
        let dir = config.bulk_dir().join("streams");
        std::fs::create_dir_all(&dir)?;
        let http = reqwest::Client::builder()
            .user_agent(format!("repoman/{}", repoman_common::VERSION))
            .build()?;
        Ok(Streams {
            config,
            dir,
            http,
            indexes: RwLock::new(HashMap::new()),
        })
    }

    pub fn get(&self, name: &str) -> Option<Arc<StreamIndex>> {
        self.indexes.read().unwrap().get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.config
            .streams_sorted()
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// Load whatever databases are already on disk, without network.
    pub fn load_cached(&self) -> Result<()> {
        for s in &self.config.streams {
            match self.build_index(s, false) {
                Ok(Some(idx)) => {
                    self.indexes
                        .write()
                        .unwrap()
                        .insert(s.name.clone(), Arc::new(idx));
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(stream = %s.name, error = %e, "cached stream index unusable"),
            }
        }
        Ok(())
    }

    /// Refresh one stream: conditional GET of every repo database, then
    /// re-parse. Returns the index and whether anything changed.
    pub async fn refresh(&self, name: &str) -> Result<(Arc<StreamIndex>, bool)> {
        let stream = self
            .config
            .stream(name)
            .with_context(|| format!("unknown stream {name}"))?
            .clone();
        let mut changed = false;
        for repo in &stream.repos {
            changed |= self.fetch_db(&stream, repo).await?;
        }
        let idx = self
            .build_index(&stream, true)?
            .context("stream has no databases after refresh")?;
        let idx = Arc::new(idx);
        self.indexes
            .write()
            .unwrap()
            .insert(name.to_string(), idx.clone());
        tracing::info!(stream = name, packages = idx.index.len(), changed, "stream refreshed");
        Ok((idx, changed))
    }

    pub async fn refresh_all(&self) -> Result<Vec<(String, bool)>> {
        let mut out = Vec::new();
        for s in self.config.streams_sorted() {
            let (_, changed) = self.refresh(&s.name).await?;
            out.push((s.name.clone(), changed));
        }
        Ok(out)
    }

    fn db_path(&self, stream: &str, repo: &str) -> PathBuf {
        self.dir.join(stream).join(format!("{repo}.db"))
    }

    /// Returns true if the file on disk changed.
    async fn fetch_db(&self, stream: &StreamConfig, repo: &str) -> Result<bool> {
        let path = self.db_path(&stream.name, repo);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let url = format!(
            "{}/{repo}.db",
            expand_server(&stream.server, repo, &self.config.arch)
        );
        let lastmod_path = path.with_extension("db.lastmod");
        let mut req = self.http.get(&url);
        if path.exists() {
            if let Ok(lm) = std::fs::read_to_string(&lastmod_path) {
                req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm.trim());
            }
        }
        let resp = req.send().await.with_context(|| format!("GET {url}"))?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(false);
        }
        if !resp.status().is_success() {
            bail!("GET {url}: {}", resp.status());
        }
        let lastmod = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = resp.bytes().await?;
        // Validate before replacing the cached copy.
        read_db(&bytes).with_context(|| format!("{url} is not a valid sync database"))?;
        let unchanged = std::fs::read(&path)
            .map(|old| old.as_slice() == bytes.as_ref())
            .unwrap_or(false);
        let tmp = path.with_extension("db.part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        if let Some(lm) = lastmod {
            std::fs::write(&lastmod_path, lm)?;
        }
        Ok(!unchanged)
    }

    fn build_index(&self, stream: &StreamConfig, require_all: bool) -> Result<Option<StreamIndex>> {
        let mut index = SyncIndex::new();
        let mut hashes = Vec::new();
        let mut any = false;
        for repo in &stream.repos {
            let path = self.db_path(&stream.name, repo);
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) if !require_all => continue,
                Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            };
            any = true;
            let pkgs = read_db(&bytes).with_context(|| format!("parsing {}", path.display()))?;
            index.add_repo(repo, pkgs);
            hashes.push((repo.clone(), hex::encode(Sha256::digest(&bytes))));
        }
        if !any {
            return Ok(None);
        }
        Ok(Some(StreamIndex {
            name: stream.name.clone(),
            index,
            fetched_at: clock::now(),
            db_hashes: hashes,
        }))
    }

    /// pacman.conf for a stream's base image: just the stream repos.
    pub fn image_pacman_conf(&self, stream: &StreamConfig) -> String {
        self.pacman_conf(stream, &[], &[]).render()
    }

    /// pacman.conf for a build root: `extra_repos` first (the staging repo),
    /// then the stream repos, with optional cache servers.
    pub fn pacman_conf(&self, stream: &StreamConfig, extra_repos: &[Repo], cache_servers: &[String]) -> PacmanConf {
        let mut repos: Vec<Repo> = extra_repos.to_vec();
        for r in &stream.repos {
            repos.push(Repo {
                name: r.clone(),
                servers: vec![stream.server.clone()],
                cache_servers: cache_servers.to_vec(),
                sig_level: None,
            });
        }
        PacmanConf {
            arch: self.config.arch.clone(),
            cache_dirs: vec!["/var/cache/pacman/pkg/".into()],
            repos,
            ..Default::default()
        }
    }
}
