//! Repos on disk: per-run staging repos for builds in progress and the
//! signed per-stream repos clients consume. Package files live in the blob
//! store; repo directories hold database files plus symlinks to blobs so
//! `repo-add` sees everything in one place and the HTTP layer can serve a
//! repo directory as a plain tree.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use repoman_common::config::CoordinatorConfig;
use tokio::process::Command;

use crate::blobs::Blobs;

pub struct Publisher {
    bulk: PathBuf,
    data: PathBuf,
    arch: String,
    signing_key: Option<String>,
}

impl Publisher {
    pub fn new(config: &CoordinatorConfig) -> Result<Self> {
        let bulk = config.bulk_dir();
        std::fs::create_dir_all(bulk.join("repos"))?;
        std::fs::create_dir_all(bulk.join("staging"))?;
        std::fs::create_dir_all(config.data_dir.join("gnupg"))?;
        Ok(Publisher {
            bulk,
            data: config.data_dir.clone(),
            arch: config.arch.clone(),
            signing_key: config.signing_key.clone(),
        })
    }

    pub fn gnupg_home(&self) -> PathBuf {
        self.data.join("gnupg")
    }

    pub fn signing_enabled(&self) -> bool {
        self.signing_key.is_some()
    }

    // ---- paths ----------------------------------------------------------------

    pub fn repo_dir(&self, stream: &str) -> PathBuf {
        self.bulk.join("repos").join(stream).join(&self.arch)
    }

    pub fn staging_dir(&self, run_id: &str, stream: &str) -> PathBuf {
        self.bulk.join("staging").join(run_id).join(stream).join(&self.arch)
    }

    pub fn staging_repo_name() -> &'static str {
        "repoman-staging"
    }

    pub fn published_repo_name(stream: &str) -> String {
        format!("repoman-{stream}")
    }

    // ---- repo-add ---------------------------------------------------------------

    async fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<String> {
        let out = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env("GNUPGHOME", self.gnupg_home())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("spawning {program}"))?;
        if !out.status.success() {
            bail!(
                "{program} {} failed: {}{}",
                args.join(" "),
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Link a blob (and its signature if present) into a repo directory.
    fn link_blob(blobs: &Blobs, sha256: &str, filename: &str, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let src = blobs.path(sha256, filename);
        anyhow::ensure!(src.exists(), "blob {} missing for {filename}", &sha256[..12]);
        let dst = dir.join(filename);
        if dst.symlink_metadata().is_ok() {
            std::fs::remove_file(&dst)?;
        }
        std::os::unix::fs::symlink(&src, &dst)?;
        let sig_src = blobs.path(sha256, &format!("{filename}.sig"));
        let sig_dst = dir.join(format!("{filename}.sig"));
        if sig_dst.symlink_metadata().is_ok() {
            std::fs::remove_file(&sig_dst)?;
        }
        if sig_src.exists() {
            std::os::unix::fs::symlink(&sig_src, &sig_dst)?;
        }
        Ok(dst)
    }

    /// Add an artifact to a run's staging repo so dependents can install it.
    pub async fn stage(&self, blobs: &Blobs, run_id: &str, stream: &str, sha256: &str, filename: &str) -> Result<()> {
        let dir = self.staging_dir(run_id, stream);
        let pkg = Self::link_blob(blobs, sha256, filename, &dir)?;
        let db = dir.join(format!("{}.db.tar.zst", Self::staging_repo_name()));
        self.run(
            "repo-add",
            &["-R".into(), "-q".into(), db.to_string_lossy().to_string(), pkg.to_string_lossy().to_string()],
            &dir,
        )
        .await?;
        Ok(())
    }

    /// Make sure a staging repo exists (possibly empty) so pacman does not
    /// fail on a missing database.
    pub async fn ensure_staging(&self, run_id: &str, stream: &str) -> Result<()> {
        let dir = self.staging_dir(run_id, stream);
        std::fs::create_dir_all(&dir)?;
        let db = dir.join(format!("{}.db.tar.zst", Self::staging_repo_name()));
        if !db.exists() {
            // repo-add with no packages creates an empty database.
            self.run("repo-add", &["-q".into(), db.to_string_lossy().to_string()], &dir).await?;
        }
        Ok(())
    }

    pub fn remove_staging(&self, run_id: &str) -> Result<()> {
        let dir = self.bulk.join("staging").join(run_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Detached-sign a blob's primary file, storing `<name>.sig` beside it.
    pub async fn sign_blob(&self, blobs: &Blobs, sha256: &str, filename: &str) -> Result<PathBuf> {
        let key = self.signing_key.as_deref().context("no signing key configured")?;
        let src = blobs.path(sha256, filename);
        let sig = blobs.path(sha256, &format!("{filename}.sig"));
        if sig.exists() {
            return Ok(sig);
        }
        let tmp = blobs.path(sha256, &format!("{filename}.sig.part"));
        self.run(
            "gpg",
            &[
                "--batch".into(),
                "--yes".into(),
                "--detach-sign".into(),
                "--no-armor".into(),
                "-u".into(),
                key.to_string(),
                "-o".into(),
                tmp.to_string_lossy().to_string(),
                src.to_string_lossy().to_string(),
            ],
            blobs.dir_of(sha256).as_path(),
        )
        .await?;
        std::fs::rename(&tmp, &sig)?;
        Ok(sig)
    }

    /// Publish artifacts to a stream repo: link blobs, add to the database
    /// (replacing older versions of the same names), sign the database.
    pub async fn publish(&self, blobs: &Blobs, stream: &str, artifacts: &[(String, String)]) -> Result<()> {
        if artifacts.is_empty() {
            return Ok(());
        }
        let dir = self.repo_dir(stream);
        std::fs::create_dir_all(&dir)?;
        let mut args: Vec<String> = vec!["-R".into(), "-q".into()];
        if self.signing_enabled() {
            args.push("-s".into());
            args.push("-v".into());
            args.push("-k".into());
            args.push(self.signing_key.clone().unwrap());
        }
        let db = dir.join(format!("{stream}.db.tar.zst"));
        args.push(db.to_string_lossy().to_string());
        for (sha, name) in artifacts {
            let p = Self::link_blob(blobs, sha, name, &dir)?;
            args.push(p.to_string_lossy().to_string());
        }
        self.run("repo-add", &args, &dir).await?;
        Ok(())
    }

    /// Remove package names from a stream repo's database and unlink files.
    pub async fn unpublish(&self, stream: &str, pkgnames: &[String], filenames: &[String]) -> Result<()> {
        if pkgnames.is_empty() {
            return Ok(());
        }
        let dir = self.repo_dir(stream);
        let db = dir.join(format!("{stream}.db.tar.zst"));
        if db.exists() {
            let mut args: Vec<String> = vec!["-q".into()];
            if self.signing_enabled() {
                args.push("-s".into());
                args.push("-k".into());
                args.push(self.signing_key.clone().unwrap());
            }
            args.push(db.to_string_lossy().to_string());
            args.extend(pkgnames.iter().cloned());
            // repo-remove exits non-zero if a name is already absent; tolerate.
            let _ = self.run("repo-remove", &args, &dir).await;
        }
        for f in filenames {
            let _ = std::fs::remove_file(dir.join(f));
            let _ = std::fs::remove_file(dir.join(format!("{f}.sig")));
        }
        Ok(())
    }

    /// Export the public signing key, armored.
    pub async fn export_public_key(&self) -> Result<String> {
        let key = self.signing_key.as_deref().context("no signing key configured")?;
        self.run("gpg", &["--batch".into(), "--armor".into(), "--export".into(), key.to_string()], &self.data)
            .await
    }

    /// Generate a signing key with no passphrase. Returns the fingerprint.
    pub async fn generate_key(&self, uid: &str) -> Result<String> {
        std::fs::set_permissions(self.gnupg_home(), std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        self.run(
            "gpg",
            &[
                "--batch".into(),
                "--passphrase".into(),
                "".into(),
                "--pinentry-mode".into(),
                "loopback".into(),
                "--quick-gen-key".into(),
                uid.to_string(),
                "ed25519".into(),
                "sign".into(),
                "never".into(),
            ],
            &self.data,
        )
        .await?;
        let out = self
            .run(
                "gpg",
                &["--batch".into(), "--with-colons".into(), "--list-secret-keys".into(), uid.to_string()],
                &self.data,
            )
            .await?;
        out.lines()
            .find(|l| l.starts_with("fpr:"))
            .and_then(|l| l.split(':').nth(9))
            .map(str::to_string)
            .context("no fingerprint in gpg output")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_and_paths() {
        let cfg = CoordinatorConfig {
            data_dir: std::env::temp_dir().join(format!("repoman-pub-{}", std::process::id())),
            ..Default::default()
        };
        let p = Publisher::new(&cfg).unwrap();
        assert_eq!(Publisher::published_repo_name("manjaro-stable"), "repoman-manjaro-stable");
        assert!(p.repo_dir("manjaro-stable").ends_with("repos/manjaro-stable/x86_64"));
        assert!(p.staging_dir("r1", "s").ends_with("staging/r1/s/x86_64"));
        assert!(!p.signing_enabled());
        let _ = std::fs::remove_dir_all(&cfg.data_dir);
    }
}
