//! Content-addressed blob store: `bulk/store/<sha256>/<filename>`. Holds
//! built packages, their detached signatures (as `<filename>.sig` beside
//! them), and base image tarballs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub struct Blobs {
    dir: PathBuf,
}

impl Blobs {
    pub fn new(bulk_dir: &Path) -> Result<Self> {
        let dir = bulk_dir.join("store");
        std::fs::create_dir_all(&dir)?;
        Ok(Blobs { dir })
    }

    pub fn dir_of(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }

    pub fn path(&self, sha256: &str, filename: &str) -> PathBuf {
        self.dir.join(sha256).join(filename)
    }

    /// The blob's primary file, if present: the one entry in its directory
    /// that is not a `.sig`.
    pub fn primary(&self, sha256: &str) -> Option<PathBuf> {
        let dir = self.dir_of(sha256);
        let mut found = None;
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "sig") || p.extension().is_some_and(|x| x == "part") {
                continue;
            }
            found = Some(p);
        }
        found
    }

    pub fn exists(&self, sha256: &str) -> bool {
        self.primary(sha256).is_some()
    }

    /// Store a stream of bytes, verifying its sha256. Rejects on mismatch
    /// and leaves nothing behind.
    pub async fn put<S, B, E>(&self, sha256: &str, filename: &str, mut body: S) -> Result<u64>
    where
        S: futures_util::Stream<Item = std::result::Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        validate_sha(sha256)?;
        validate_filename(filename)?;
        let dir = self.dir_of(sha256);
        tokio::fs::create_dir_all(&dir).await?;
        let final_path = dir.join(filename);
        if final_path.exists() {
            // Idempotent re-upload: consume and discard.
            let mut n = 0u64;
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| anyhow::anyhow!("{e}"))?;
                n += chunk.as_ref().len() as u64;
            }
            return Ok(n);
        }
        let part = dir.join(format!("{filename}.part"));
        let mut out = tokio::fs::File::create(&part).await?;
        let mut h = Sha256::new();
        let mut n = 0u64;
        let result: Result<()> = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(|e| anyhow::anyhow!("{e}"))?;
                h.update(chunk.as_ref());
                out.write_all(chunk.as_ref()).await?;
                n += chunk.as_ref().len() as u64;
            }
            out.flush().await?;
            Ok(())
        }
        .await;
        drop(out);
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        let got = hex::encode(h.finalize());
        if got != sha256 {
            let _ = tokio::fs::remove_file(&part).await;
            let _ = tokio::fs::remove_dir(&dir).await;
            bail!("checksum mismatch: declared {sha256}, got {got}");
        }
        tokio::fs::rename(&part, &final_path).await?;
        Ok(n)
    }

    /// Store bytes already in memory (signatures, small files).
    pub fn put_sibling(&self, sha256: &str, filename: &str, bytes: &[u8]) -> Result<PathBuf> {
        validate_filename(filename)?;
        let dir = self.dir_of(sha256);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(filename);
        let part = dir.join(format!("{filename}.part"));
        std::fs::write(&part, bytes)?;
        std::fs::rename(&part, &path)?;
        Ok(path)
    }

    pub fn delete(&self, sha256: &str) -> Result<()> {
        let dir = self.dir_of(sha256);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn total_size(&self) -> Result<u64> {
        let mut total = 0;
        for e in std::fs::read_dir(&self.dir)?.flatten() {
            if let Ok(inner) = std::fs::read_dir(e.path()) {
                for f in inner.flatten() {
                    total += f.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        Ok(total)
    }
}

pub fn validate_sha(s: &str) -> Result<()> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        bail!("invalid sha256 {s:?}");
    }
    Ok(())
}

pub fn validate_filename(s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > 255
        || s.contains('/')
        || s.contains('\\')
        || s == "."
        || s == ".."
        || s.starts_with('.')
        || s.ends_with(".part")
        || s.bytes().any(|b| b < 0x20)
    {
        bail!("invalid filename {s:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_of(bytes: &'static [u8]) -> impl futures_util::Stream<Item = std::result::Result<&'static [u8], String>> + Unpin {
        futures_util::stream::iter(vec![Ok(bytes)])
    }

    #[tokio::test]
    async fn put_verifies_and_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let b = Blobs::new(d.path()).unwrap();
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(b.put(sha, "abc.txt", stream_of(b"abc")).await.unwrap(), 3);
        assert!(b.exists(sha));
        assert_eq!(std::fs::read(b.primary(sha).unwrap()).unwrap(), b"abc");
        assert_eq!(b.put(sha, "abc.txt", stream_of(b"abc")).await.unwrap(), 3);
        let bad = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(b.put(bad, "x", stream_of(b"abc")).await.is_err());
        assert!(!b.exists(bad));
        b.put_sibling(sha, "abc.txt.sig", b"sig").unwrap();
        assert_eq!(b.primary(sha).unwrap().file_name().unwrap(), "abc.txt");
        assert_eq!(b.total_size().unwrap(), 6);
        b.delete(sha).unwrap();
        assert!(!b.exists(sha));
    }

    #[test]
    fn validates_names() {
        assert!(validate_filename("foo-1.0-1-x86_64.pkg.tar.zst").is_ok());
        assert!(validate_filename("../x").is_err());
        assert!(validate_filename(".hidden").is_err());
        assert!(validate_filename("a/b").is_err());
        assert!(validate_sha("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").is_ok());
        assert!(validate_sha("BA78").is_err());
    }
}
