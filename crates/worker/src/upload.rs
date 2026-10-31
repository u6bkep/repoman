//! Content-addressed blob upload and download against the coordinator.

use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex::encode(h.finalize()), n))
}

/// `PUT {upload_url}/{sha256}?name={filename}` streaming the file body.
pub async fn put_blob(http: &reqwest::Client, upload_url: &str, token: &str, path: &Path, sha256: &str, filename: &str) -> Result<()> {
    let file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let url = format!("{}/{sha256}?name={}", upload_url.trim_end_matches('/'), urlencoding(filename));
    let resp = http
        .put(&url)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_LENGTH, len)
        .body(body)
        .send()
        .await
        .with_context(|| format!("PUT {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("upload of {filename} rejected: {status} {text}");
    }
    Ok(())
}

/// Download to `dest` (via a `.part` file), verifying the sha256.
pub async fn get_blob(http: &reqwest::Client, url: &str, expected_sha256: &str, dest: &Path) -> Result<()> {
    let part = dest.with_extension("part");
    if let Some(p) = dest.parent() {
        tokio::fs::create_dir_all(p).await?;
    }
    let mut resp = http.get(url).send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: {}", resp.status());
    }
    let mut out = tokio::fs::File::create(&part).await?;
    let mut h = Sha256::new();
    while let Some(chunk) = resp.chunk().await? {
        h.update(&chunk);
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    drop(out);
    let got = hex::encode(h.finalize());
    if got != expected_sha256 {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("checksum mismatch for {url}: expected {expected_sha256}, got {got}");
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(())
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'+' | b':' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        std::fs::write(&p, b"abc").unwrap();
        let (h, n) = sha256_file(&p).unwrap();
        assert_eq!(n, 3);
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
