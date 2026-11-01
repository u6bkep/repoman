//! The build executor: provision, deps, sources, build, collect.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use repoman_alpm::pkginfo::read_package_meta;
use repoman_common::proto::{ArtifactInfo, Fd, ImageRef, JobResult};

use crate::exec::{JobCtx, run_logged};
use crate::image::{ensure_image, makepkg_conf};
use crate::sandbox::{RootSource, RunAs, Sandbox};
use crate::upload;

pub struct BuildParams<'a> {
    pub pkgbase: &'a str,
    pub image: &'a ImageRef,
    pub source_url: &'a str,
    pub pacman_conf: &'a str,
    pub install: &'a [String],
    pub pgp_keys: &'a [String],
    pub skip_pgp: bool,
    pub allow_network: bool,
    pub upload_url: &'a str,
    pub makepkg_conf_extra: &'a str,
}

/// Paths under the overlay upper that a build is allowed to write.
const ALLOWED_WRITE_PREFIXES: &[&str] = &[
    "build/",
    "srcdest/",
    "tmp/",
    "var/tmp/",
    "run/",
    "root/.gnupg/",
    "root/.cache/",
    "etc/resolv.conf",
    "etc/hosts",
    "etc/pacman.conf",
    "etc/makepkg.conf.d/",
    "etc/ld.so.cache",
];

pub async fn run_build(ctx: &JobCtx, p: BuildParams<'_>) -> Result<JobResult> {
    let cfg = &ctx.config;
    let job_dir = ctx.job_dir();
    let upper = job_dir.join("upper");
    let work = job_dir.join("work");
    let mnt = job_dir.join("root");
    let build_dir = job_dir.join("build");
    let srcdest = cfg.scratch_dir.join("srcdest");
    let pkgcache = cfg.scratch_dir.join("pkgcache");
    let uid = cfg.build_uid;

    // ---- provision --------------------------------------------------------
    ctx.stage("provision");
    let lower = ensure_image(ctx, p.image).await?;
    for d in [&upper, &work, &mnt, &build_dir, &srcdest, &pkgcache] {
        tokio::fs::create_dir_all(d).await?;
    }
    for d in ["pkgdest", "srcpkgdest", "logs", "work", "keys"] {
        tokio::fs::create_dir_all(build_dir.join(d)).await?;
    }
    // Fetch the reviewed source tree.
    let src_tar = job_dir.join("source.tar");
    download(&ctx.http, p.source_url, &src_tar).await?;
    let mut cmd = tokio::process::Command::new("bsdtar");
    cmd.arg("-xf").arg(&src_tar).arg("-C").arg(&build_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let (status, tail) = run_logged(ctx, cmd, None).await?;
    if !status.success() {
        bail!("extracting source tarball failed: {}", tail.join(" | "));
    }
    let pkgdir_host = build_dir.join(p.pkgbase);
    if !pkgdir_host.join("PKGBUILD").is_file() {
        bail!("source tarball has no {}/PKGBUILD", p.pkgbase);
    }
    for (i, key) in p.pgp_keys.iter().enumerate() {
        tokio::fs::write(build_dir.join("keys").join(format!("{i}.asc")), key).await?;
    }
    // Pre-seed the upper with the job's pacman and makepkg configuration.
    tokio::fs::create_dir_all(upper.join("etc/makepkg.conf.d")).await?;
    tokio::fs::write(upper.join("etc/pacman.conf"), p.pacman_conf).await?;
    tokio::fs::write(
        upper.join("etc/makepkg.conf.d/repoman.conf"),
        makepkg_conf(uid, p.makepkg_conf_extra),
    )
    .await?;
    chown_recursive(&build_dir, uid)?;
    chown_recursive(&srcdest, uid)?;

    let sandbox = |network: bool| Sandbox {
        root: RootSource::Overlay {
            lower: lower.clone(),
            upper: upper.clone(),
            work: work.clone(),
        },
        mountpoint: mnt.clone(),
        binds: vec![
            (build_dir.clone(), "/build".into()),
            (srcdest.clone(), "/srcdest".into()),
            (pkgcache.clone(), "/var/cache/pacman/pkg".into()),
        ],
        network,
        cgroup: ctx.cgroup_path(),
        hostname: "repoman-build".into(),
        chroot: true,
        netlog: Some(job_dir.join("netlog.json")),
    };
    sandbox(true).prepare_dirs()?;
    ctx.stage_done("provision");

    // ---- deps -------------------------------------------------------------
    ctx.stage("deps");
    let mut argv = vec![
        "pacman".to_string(),
        "-Syu".into(),
        "--noconfirm".into(),
        "--needed".into(),
    ];
    argv.extend(p.install.iter().cloned());
    ctx.log(Fd::System, format!("installing {} dependencies", p.install.len()));
    let cmd = sandbox(true).command(&argv, RunAs::Root, "/", &[])?;
    let (mut status, mut tail) = {
        let _cache_guard = ctx.lock_pacman().await;
        run_logged(ctx, cmd, Some(ctx.timeout())).await?
    };
    if !status.success() && tail.iter().any(|l| l.contains("invalid or corrupted package")) {
        // The shared cache held a file with this name from another stream or
        // an earlier rebuild: same pkgver, different bytes. pacman has already
        // deleted it (the delete prompt defaults to yes under --noconfirm),
        // so a second attempt downloads the right one.
        ctx.log(Fd::System, "a cached package failed its checksum; retrying after pacman dropped it".to_string());
        let cmd = sandbox(true).command(&argv, RunAs::Root, "/", &[])?;
        (status, tail) = {
            let _cache_guard = ctx.lock_pacman().await;
            run_logged(ctx, cmd, Some(ctx.timeout())).await?
        };
    }
    if !status.success() {
        bail!("dependency installation failed ({status}): {}", tail.join(" | "));
    }
    ctx.stage_done("deps");

    let user = RunAs::User { uid, gid: uid };
    let user_env = vec![
        ("HOME".to_string(), "/build".to_string()),
        ("GNUPGHOME".to_string(), "/build/.gnupg".to_string()),
        ("USER".to_string(), "builder".to_string()),
        ("LOGNAME".to_string(), "builder".to_string()),
    ];
    let cwd = format!("/build/{}", p.pkgbase);
    let mut pgp_flag: Vec<String> = Vec::new();
    if p.skip_pgp {
        pgp_flag.push("--skippgpcheck".into());
    }

    // ---- sources ----------------------------------------------------------
    ctx.stage("sources");
    if !p.pgp_keys.is_empty() {
        let script = "mkdir -p -m 700 /build/.gnupg && gpg --batch --quiet --import /build/keys/*.asc";
        let cmd = sandbox(true).command(
            &["/bin/sh".into(), "-c".into(), script.into()],
            user,
            &cwd,
            &user_env,
        )?;
        let (status, tail) = run_logged(ctx, cmd, Some(ctx.timeout())).await?;
        if !status.success() {
            bail!("importing PGP keys failed: {}", tail.join(" | "));
        }
    }
    // Download, verify, extract, and run prepare() with the network up.
    // Arch's packaging guidelines put dependency fetching (cargo fetch, go
    // mod download, npm ci) in prepare(), so it belongs to this phase; only
    // build(), check(), and package() run offline.
    let mut argv = vec!["makepkg".to_string(), "--nobuild".into(), "--noconfirm".into()];
    argv.extend(pgp_flag.iter().cloned());
    let cmd = sandbox(true).command(&argv, user, &cwd, &user_env)?;
    let (status, tail) = run_logged(ctx, cmd, Some(ctx.timeout())).await?;
    if !status.success() {
        bail!("source download, verification, or prepare() failed ({status}): {}", tail.join(" | "));
    }
    ctx.stage_done("sources");

    // ---- build (offline unless policy allows network) ----------------------
    ctx.stage("build");
    if p.allow_network {
        ctx.log(Fd::System, "policy allows network during build; this build is not isolated from the network");
    }
    let before = snapshot_upper(&upper)?;
    // --noextract skips download, verification, extraction, and prepare(),
    // all done above; --holdver keeps VCS sources as fetched.
    let mut argv = vec![
        "makepkg".to_string(),
        "--noextract".into(),
        "--holdver".into(),
        "--noconfirm".into(),
        "--nosign".into(),
    ];
    argv.extend(pgp_flag.iter().cloned());
    let cmd = sandbox(p.allow_network).command(&argv, user, &cwd, &user_env)?;
    let (status, tail) = run_logged(ctx, cmd, Some(ctx.timeout())).await?;
    let oom = ctx.oom_killed();
    if !status.success() {
        if oom {
            bail!("build killed by memory limit");
        }
        bail!("makepkg failed ({status}): {}", tail.join(" | "));
    }
    let after = snapshot_upper(&upper)?;
    let outside_writes = outside_writes(&before, &after);
    for w in &outside_writes {
        ctx.log(Fd::System, format!("wrote outside expected paths: /{w}"));
    }
    let net_attempts = if p.allow_network {
        0
    } else {
        let n = read_netlog(&job_dir.join("netlog.json"));
        if n > 0 {
            ctx.log(Fd::System, format!("{n} packets tried to leave the offline build (counted and rejected)"));
        }
        n
    };
    ctx.stage_done("build");

    // ---- collect ----------------------------------------------------------
    ctx.stage("collect");
    let pkgdest = build_dir.join("pkgdest");
    let mut artifacts = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&pkgdest)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.contains(".pkg.tar.") && !f.ends_with(".sig"))
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        bail!("makepkg produced no packages in pkgdest");
    }
    for path in entries {
        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(&path)?;
        let meta = read_package_meta(&bytes).with_context(|| format!("reading {filename}"))?;
        let info = meta.pkginfo.context("package without PKGINFO")?;
        let (sha256, size) = upload::sha256_file(&path)?;
        ctx.log(Fd::System, format!("built {filename} ({} KiB)", size / 1024));
        upload::put_blob(&ctx.http, p.upload_url, &cfg.token, &path, &sha256, &filename).await?;
        let buildinfo = meta
            .buildinfo
            .map(|b| serde_json::to_value(b.to_map()).unwrap_or_default())
            .unwrap_or(serde_json::json!({}));
        artifacts.push(ArtifactInfo {
            sha256,
            filename,
            size,
            pkgname: info.pkgname.clone(),
            pkgbase: info.pkgbase.clone(),
            pkgver: info.pkgver.clone(),
            arch: info.arch.clone(),
            depends: info.depends.clone(),
            provides: info.provides.clone(),
            paths: meta.paths,
            has_install_script: meta.install_script.is_some(),
            setuid_paths: meta.setuid_paths,
            buildinfo,
        });
    }
    // Scratch usage: the upper plus the job's build directory.
    let scratch = dir_size(&upper).unwrap_or(0) + dir_size(&build_dir).unwrap_or(0);
    ctx.set_scratch_peak(scratch);
    // makepkg extracts into BUILDDIR (/build/work/<pkgbase>/src), not next to
    // the PKGBUILD.
    let upstream_rev = read_vcs_rev(&build_dir.join("work").join(p.pkgbase));
    ctx.stage_done("collect");

    Ok(JobResult::Build {
        artifacts,
        net_attempts,
        outside_writes,
        upstream_rev,
    })
}

/// Packets the sandbox's nftables counter saw on the sink interface, from
/// the `nft -j list counter` dump the setup script leaves behind. Zero when
/// the file is missing (no nft, no dummy module) or unparseable.
pub fn read_netlog(path: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    v["nftables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["counter"]["packets"].as_u64())
        .sum()
}

/// Regenerate SRCINFO from the PKGBUILD, offline, as the build user.
pub async fn run_inspect(ctx: &JobCtx, pkgbase: &str, image: &ImageRef, source_url: &str) -> Result<JobResult> {
    let cfg = &ctx.config;
    let job_dir = ctx.job_dir();
    let upper = job_dir.join("upper");
    let work = job_dir.join("work");
    let mnt = job_dir.join("root");
    let build_dir = job_dir.join("build");
    ctx.stage("provision");
    let lower = ensure_image(ctx, image).await?;
    for d in [&upper, &work, &mnt, &build_dir] {
        tokio::fs::create_dir_all(d).await?;
    }
    let src_tar = job_dir.join("source.tar");
    download(&ctx.http, source_url, &src_tar).await?;
    let mut cmd = tokio::process::Command::new("bsdtar");
    cmd.arg("-xf").arg(&src_tar).arg("-C").arg(&build_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let (status, tail) = run_logged(ctx, cmd, None).await?;
    if !status.success() {
        bail!("extracting source tarball failed: {}", tail.join(" | "));
    }
    chown_recursive(&build_dir, cfg.build_uid)?;
    let sb = Sandbox {
        root: RootSource::Overlay {
            lower,
            upper,
            work,
        },
        mountpoint: mnt,
        binds: vec![(build_dir.clone(), "/build".into())],
        network: false,
        cgroup: ctx.cgroup_path(),
        hostname: "repoman-inspect".into(),
        chroot: true,
        netlog: None,
    };
    sb.prepare_dirs()?;
    ctx.stage_done("provision");

    ctx.stage("printsrcinfo");
    let out_path = build_dir.join("SRCINFO.generated");
    let script = "makepkg --printsrcinfo > /build/SRCINFO.generated";
    let cmd = sb.command(
        &["/bin/sh".into(), "-c".into(), script.into()],
        RunAs::User { uid: cfg.build_uid, gid: cfg.build_uid },
        &format!("/build/{pkgbase}"),
        &[("HOME".into(), "/build".into())],
    )?;
    let (status, tail) = run_logged(ctx, cmd, Some(std::time::Duration::from_secs(600))).await?;
    if !status.success() {
        bail!("makepkg --printsrcinfo failed ({status}): {}", tail.join(" | "));
    }
    let srcinfo = std::fs::read_to_string(&out_path)?;
    ctx.stage_done("printsrcinfo");
    Ok(JobResult::Inspect { srcinfo })
}

async fn download(http: &reqwest::Client, url: &str, dest: &Path) -> Result<()> {
    let resp = http.get(url).send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: {}", resp.status());
    }
    let bytes = resp.bytes().await?;
    tokio::fs::write(dest, &bytes).await?;
    Ok(())
}

fn chown_recursive(path: &Path, uid: u32) -> Result<()> {
    // chown is needed only when running as root; otherwise files are ours.
    if !crate::sandbox::is_root() {
        return Ok(());
    }
    let status = std::process::Command::new("chown")
        .arg("-R")
        .arg(format!("{uid}:{uid}"))
        .arg(path)
        .status()?;
    anyhow::ensure!(status.success(), "chown {} failed", path.display());
    Ok(())
}

/// Relative path → (mtime nanos, size) for every file under `upper`.
fn snapshot_upper(upper: &Path) -> Result<BTreeMap<String, (i128, u64)>> {
    let mut out = BTreeMap::new();
    walk(upper, upper, &mut |rel, md| {
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        out.insert(rel.to_string(), (mtime, md.len()));
    })?;
    Ok(out)
}

fn walk(base: &Path, dir: &Path, f: &mut dyn FnMut(&str, &std::fs::Metadata)) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let md = entry.metadata()?;
        if md.is_dir() {
            walk(base, &path, f)?;
        } else {
            let rel = path.strip_prefix(base).unwrap().to_string_lossy().to_string();
            f(&rel, &md);
        }
    }
    Ok(())
}

/// Files new or modified in `after` versus `before`, outside allowed paths.
pub fn outside_writes(before: &BTreeMap<String, (i128, u64)>, after: &BTreeMap<String, (i128, u64)>) -> Vec<String> {
    after
        .iter()
        .filter(|(path, stamp)| before.get(*path) != Some(stamp))
        .map(|(path, _)| path.clone())
        .filter(|path| !ALLOWED_WRITE_PREFIXES.iter().any(|p| path.starts_with(p) || path == p.trim_end_matches('/')))
        .collect()
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0;
    walk(path, path, &mut |_, md| total += md.len())?;
    Ok(total)
}

/// For VCS packages makepkg leaves the checkout under `<builddir>/src/`;
/// record the HEAD of the first git checkout found there.
fn read_vcs_rev(builddir: &Path) -> Option<String> {
    let src = builddir.join("src");
    let entries = std::fs::read_dir(src).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.join(".git").exists() {
            // The checkout belongs to the build uid; git refuses to read
            // repositories owned by another user unless told otherwise.
            let out = std::process::Command::new("git")
                .args(["-c", "safe.directory=*", "rev-parse", "HEAD"])
                .current_dir(&p)
                .output()
                .ok()?;
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn netlog_counts_packets() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("netlog.json");
        assert_eq!(read_netlog(&f), 0, "missing file");
        std::fs::write(&f, r#"{"nftables":[{"metainfo":{"version":"1.1"}},{"counter":{"family":"inet","name":"attempts","table":"repoman","handle":1,"packets":7,"bytes":420}}]}"#).unwrap();
        assert_eq!(read_netlog(&f), 7);
        std::fs::write(&f, "garbage").unwrap();
        assert_eq!(read_netlog(&f), 0);
    }

    use super::*;

    #[test]
    fn outside_writes_filters_allowed_paths() {
        let mut before = BTreeMap::new();
        before.insert("usr/lib/libx.so".to_string(), (1, 10));
        before.insert("etc/passwd".to_string(), (1, 10));
        let mut after = before.clone();
        after.insert("build/foo/src/x.c".into(), (2, 5));
        after.insert("srcdest/x.tar.gz".into(), (2, 5));
        after.insert("tmp/t".into(), (2, 5));
        after.insert("etc/passwd".into(), (9, 11)); // modified
        after.insert("usr/local/bin/evil".into(), (2, 5)); // new
        after.insert("root/.bashrc".into(), (2, 5));
        let w = outside_writes(&before, &after);
        assert_eq!(w, ["etc/passwd", "root/.bashrc", "usr/local/bin/evil"]);
    }
}
