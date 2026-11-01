//! Pull-through cache for upstream package files, consumed through pacman's
//! `CacheServer` by workers and clients. A hit is served from disk with
//! range support; a miss is fetched from the stream mirror that lists the
//! file, verified against the sync database's size and sha256, renamed into
//! place, and then served. Files that leave every stream database get a
//! grace period, after which they are deleted; a size cap evicts the least
//! recently served of those early. Serving never depends on the referenced
//! set, only eviction does.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use repoman_common::config::CoordinatorConfig;
use repoman_store::Store;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use repoman_alpm::pacman_conf::expand_server;

use crate::streams::Streams;

pub struct Cache {
    dir: PathBuf,
    config: Arc<CoordinatorConfig>,
    streams: Arc<Streams>,
    store: Arc<Store>,
    http: reqwest::Client,
    locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub errors: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheView {
    pub enabled: bool,
    pub url: String,
    pub grace_days: u64,
    pub max_bytes: Option<u64>,
    pub hits: u64,
    pub misses: u64,
    pub errors: u64,
    #[serde(flatten)]
    pub stats: repoman_store::cache::CacheStats,
}

/// Only plain package files and their signatures, no path components.
pub fn valid_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b':' | b'@' | b'~'))
        && (name.contains(".pkg.tar.") || name.ends_with(".sig"))
        && !name.ends_with(".db") && !name.ends_with(".files")
}

impl Cache {
    pub fn new(config: Arc<CoordinatorConfig>, streams: Arc<Streams>, store: Arc<Store>) -> Result<Self> {
        let dir = config.bulk_dir().join("cache").join("pkg");
        std::fs::create_dir_all(&dir)?;
        let http = reqwest::Client::builder()
            .user_agent(format!("repoman/{}", repoman_common::VERSION))
            .build()?;
        Ok(Cache {
            dir,
            config,
            streams,
            store,
            http,
            locks: std::sync::Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        })
    }

    pub fn url(&self) -> String {
        format!("{}/cache", self.config.public_url.trim_end_matches('/'))
    }

    /// CacheServer list for generated pacman.conf files, empty when disabled.
    pub fn servers(&self) -> Vec<String> {
        if self.config.cache_enabled { vec![self.url()] } else { Vec::new() }
    }

    fn path(&self, filename: &str) -> PathBuf {
        self.dir.join(filename)
    }

    fn lock_for(&self, filename: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(filename.to_string()).or_default().clone()
    }

    /// Where a filename comes from: (stream, repo, expected size, expected
    /// sha256). Signatures resolve through their package. Unknown files
    /// yield every stream's repos as candidates with nothing to verify.
    fn resolve(&self, filename: &str) -> Vec<(String, String, Option<u64>, Option<String>)> {
        let base = filename.strip_suffix(".sig").unwrap_or(filename);
        let is_sig = base != filename;
        let mut out = Vec::new();
        for s in self.config.streams_sorted() {
            if let Some(ix) = self.streams.get(&s.name)
                && let Some((repo, pkg)) = ix.index.by_filename(base)
            {
                let (size, sha) = if is_sig { (None, None) } else { (pkg.csize, pkg.sha256sum.clone()) };
                out.push((s.name.clone(), repo.to_string(), size, sha));
            }
        }
        if out.is_empty() {
            for s in self.config.streams_sorted() {
                for repo in &s.repos {
                    out.push((s.name.clone(), repo.clone(), None, None));
                }
            }
        }
        out
    }

    async fn fetch(&self, filename: &str) -> Result<()> {
        let candidates = self.resolve(filename);
        let final_path = self.path(filename);
        let tmp = self.dir.join(format!(".{filename}.part-{}", repoman_common::ids::new_id()));
        let mut last_err = None;
        for (stream, repo, size, sha) in candidates {
            let Some(sc) = self.config.stream(&stream) else { continue };
            let url = format!("{}/{filename}", expand_server(&sc.server, &repo, &self.config.arch));
            match self.download(&url, &tmp, size, sha.as_deref()).await {
                Ok(n) => {
                    tokio::fs::rename(&tmp, &final_path).await?;
                    self.store.cache_insert(filename, n, sha.as_deref(), Some(&stream), Some(&repo))?;
                    tracing::info!(filename, %url, bytes = n, "cached");
                    return Ok(());
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    tracing::debug!(filename, %url, error = %e, "cache fetch attempt failed");
                    last_err = Some(e);
                }
            }
        }
        bail!("{filename}: {}", last_err.map(|e| format!("{e:#}")).unwrap_or_else(|| "no mirror lists this file".into()))
    }

    async fn download(&self, url: &str, tmp: &Path, size: Option<u64>, sha: Option<&str>) -> Result<u64> {
        let resp = self.http.get(url).send().await.context("request")?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let mut file = tokio::fs::File::create(tmp).await?;
        let mut hasher = Sha256::new();
        let mut n = 0u64;
        let mut stream = resp.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("body")?;
            hasher.update(&chunk);
            n += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if let Some(s) = size
            && s != n
        {
            bail!("size {n} differs from the database's {s}");
        }
        if let Some(expected) = sha {
            let got = hex::encode(hasher.finalize());
            if !got.eq_ignore_ascii_case(expected) {
                bail!("sha256 {got} differs from the database's {expected}");
            }
        }
        Ok(n)
    }

    /// Serve `/cache/{filename}` for the given request (ranges honoured).
    pub async fn serve(&self, filename: &str, req: Request<Body>) -> Response {
        if !self.config.cache_enabled {
            return (StatusCode::NOT_FOUND, "cache disabled").into_response();
        }
        if !valid_filename(filename) {
            return (StatusCode::NOT_FOUND, "not a package file").into_response();
        }
        let path = self.path(filename);
        if !path.is_file() {
            let lock = self.lock_for(filename);
            let _g = lock.lock().await;
            if !path.is_file() {
                self.misses.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = self.fetch(filename).await {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(filename, error = %e, "cache miss could not be filled");
                    // pacman falls back to Server on anything but success.
                    return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response();
                }
            }
        } else {
            self.hits.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.store.cache_touch(filename);
        match ServeFile::new(&path).oneshot(req).await {
            Ok(r) => r.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }

    /// Every filename (and its signature) listed by any stream database.
    fn referenced_set(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        for s in self.config.streams_sorted() {
            if let Some(ix) = self.streams.get(&s.name) {
                for f in ix.index.filenames() {
                    set.insert(f.to_string());
                    set.insert(format!("{f}.sig"));
                }
            }
        }
        set
    }

    /// Reconcile the referenced set, delete files past their grace period,
    /// then enforce the size cap on what is left evictable. Returns
    /// (deleted files, freed bytes).
    pub fn maintain(&self) -> Result<(usize, u64)> {
        let referenced = self.referenced_set();
        let (newly, back) = self.store.cache_mark_referenced(&referenced)?;
        if newly + back > 0 {
            tracing::info!(newly_unreferenced = newly, referenced_again = back, "cache reference set reconciled");
        }
        let cutoff = {
            let secs = self.config.cache_grace_days * 86_400;
            let t = time::OffsetDateTime::now_utc() - time::Duration::seconds(secs as i64);
            repoman_common::clock::format(t)
        };
        let mut deleted = 0usize;
        let mut freed = 0u64;
        for f in self.store.cache_expired(&cutoff)? {
            if self.remove(&f.filename)? {
                deleted += 1;
                freed += f.size;
            }
        }
        if let Some(cap) = self.config.cache_max_bytes {
            let mut total = self.store.cache_stats()?.bytes;
            for f in self.store.cache_evictable_lru()? {
                if total <= cap {
                    break;
                }
                if self.remove(&f.filename)? {
                    deleted += 1;
                    freed += f.size;
                    total = total.saturating_sub(f.size);
                }
            }
            if total > cap {
                tracing::warn!(total, cap, "cache above its size cap with nothing evictable; referenced files are never evicted");
            }
        }
        // Files on disk without a row (interrupted downloads) are junk.
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && name.contains(".part-")
                && let Ok(m) = e.metadata()
                && m.modified().ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 3600)
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
        if deleted > 0 {
            tracing::info!(deleted, freed, "cache eviction");
        }
        Ok((deleted, freed))
    }

    fn remove(&self, filename: &str) -> Result<bool> {
        let path = self.path(filename);
        let existed = path.is_file();
        if existed {
            std::fs::remove_file(&path)?;
        }
        self.store.cache_delete(filename)?;
        Ok(existed)
    }

    pub fn view(&self) -> Result<CacheView> {
        Ok(CacheView {
            enabled: self.config.cache_enabled,
            url: self.url(),
            grace_days: self.config.cache_grace_days,
            max_bytes: self.config.cache_max_bytes,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            stats: self.store.cache_stats()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_package_files_only() {
        assert!(valid_filename("glibc-2.44-1-x86_64.pkg.tar.zst"));
        assert!(valid_filename("glibc-2.44-1-x86_64.pkg.tar.zst.sig"));
        assert!(valid_filename("lib32-foo-1:2.0-1-any.pkg.tar.xz"));
        assert!(!valid_filename("core.db"));
        assert!(!valid_filename("../etc/passwd"));
        assert!(!valid_filename("a/b.pkg.tar.zst"));
        assert!(!valid_filename(".hidden.pkg.tar.zst"));
        assert!(!valid_filename("core.files"));
    }
}
