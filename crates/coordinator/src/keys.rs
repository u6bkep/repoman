//! PGP key cache for `validpgpkeys`. Keys are fetched once into a dedicated
//! keyring and exported armored for the worker's per-job GNUPGHOME.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

pub struct Keys {
    home: PathBuf,
    keyservers: Vec<String>,
}

impl Keys {
    pub fn new(data_dir: &std::path::Path) -> Result<Self> {
        let home = data_dir.join("gnupg-keys");
        std::fs::create_dir_all(&home)?;
        std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        Ok(Keys {
            home,
            keyservers: vec![
                "hkps://keyserver.ubuntu.com".into(),
                "hkps://keys.openpgp.org".into(),
            ],
        })
    }

    async fn gpg(&self, args: &[&str]) -> Result<std::process::Output> {
        Command::new("gpg")
            .arg("--batch")
            .arg("--no-tty")
            .args(args)
            .env("GNUPGHOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("spawning gpg")
    }

    pub async fn has(&self, fingerprint: &str) -> bool {
        self.gpg(&["--list-keys", fingerprint])
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Fetch a key by fingerprint from the configured keyservers.
    pub async fn fetch(&self, fingerprint: &str) -> Result<()> {
        if self.has(fingerprint).await {
            return Ok(());
        }
        let mut errors = Vec::new();
        for ks in &self.keyservers {
            let out = self.gpg(&["--keyserver", ks, "--recv-keys", fingerprint]).await?;
            if out.status.success() && self.has(fingerprint).await {
                tracing::info!(%fingerprint, keyserver = %ks, "fetched PGP key");
                return Ok(());
            }
            errors.push(format!("{ks}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        bail!("could not fetch key {fingerprint}: {}", errors.join("; "))
    }

    /// Armored export of the given fingerprints, fetching any that are
    /// missing. Returns one armored block per key.
    pub async fn export(&self, fingerprints: &[String]) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for fpr in fingerprints {
            let fpr = fpr.trim();
            if fpr.is_empty() {
                continue;
            }
            self.fetch(fpr).await?;
            let o = self.gpg(&["--armor", "--export", fpr]).await?;
            if !o.status.success() || o.stdout.is_empty() {
                bail!("exporting key {fpr} failed: {}", String::from_utf8_lossy(&o.stderr).trim());
            }
            out.push(String::from_utf8_lossy(&o.stdout).to_string());
        }
        Ok(out)
    }

    /// Import an armored key supplied by the operator.
    pub async fn import(&self, armored: &str) -> Result<()> {
        let mut child = Command::new("gpg")
            .args(["--batch", "--no-tty", "--import"])
            .env("GNUPGHOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(armored.as_bytes()).await?;
        }
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("gpg --import failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}
