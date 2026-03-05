use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{Config, ResolvedCacheConfig};
use crate::models::SrcInfo;

/// Build script with verbose progress output
/// Key improvements:
/// - Clear section headers for each phase
/// - Shows package info before building
/// - Uses --noprogressbar since terminal progress bars don't work in piped output
/// - Explicit status messages at key points
const BUILD_SCRIPT: &str = r#"
set -e

# Configure local repository at runtime (injected via env var)
if [ -n "$REPOMAN_REPO_NAME" ]; then
    printf '\n[%s]\nSigLevel = Optional TrustAll\nServer = file:///repo\n' "$REPOMAN_REPO_NAME" | sudo tee -a /etc/pacman.conf > /dev/null
fi

echo ""
echo "╔════════════════════════════════════════════════════════════════╗"
echo "║  SYNCHRONIZING PACKAGE DATABASES                               ║"
echo "╚════════════════════════════════════════════════════════════════╝"
echo ""
# --noprogressbar shows text-based progress instead of terminal progress bars
sudo pacman -Sy --noconfirm --noprogressbar
echo ""
echo "✓ Database sync complete"

echo ""
echo "╔════════════════════════════════════════════════════════════════╗"
echo "║  PREPARING BUILD ENVIRONMENT                                   ║"
echo "╚════════════════════════════════════════════════════════════════╝"
echo ""
mkdir -p /build/work
echo "Copying source files..."
cp -r /build/src/* /build/work/
cd /build/work

# Show what we're building
if [ -f PKGBUILD ]; then
    # shellcheck disable=SC1091
    source PKGBUILD 2>/dev/null || true
    echo ""
    echo "Package information:"
    echo "  pkgbase:       ${pkgbase:-${pkgname:-unknown}}"
    echo "  pkgver:        ${pkgver:-?}"
    echo "  pkgrel:        ${pkgrel:-?}"
    [ -n "${pkgdesc}" ] && echo "  description:   ${pkgdesc}"
    [ -n "${url}" ] && echo "  url:           ${url}"
    [ -n "${arch[*]}" ] && echo "  arch:          ${arch[*]}"
    echo ""
    if [ -n "${depends[*]}" ]; then
        echo "Runtime dependencies:"
        printf '  - %s\n' "${depends[@]}"
        echo ""
    fi
    if [ -n "${makedepends[*]}" ]; then
        echo "Build dependencies:"
        printf '  - %s\n' "${makedepends[@]}"
        echo ""
    fi
fi
echo "✓ Build environment ready"

echo ""
echo "╔════════════════════════════════════════════════════════════════╗"
echo "║  BUILDING PACKAGE                                              ║"
echo "╚════════════════════════════════════════════════════════════════╝"
echo ""
echo "This will:"
echo "  1. Install missing dependencies (if any)"
echo "  2. Download sources (if not cached)"
echo "  3. Verify source checksums"
echo "  4. Build the package"
echo "  5. Create package archive"
echo ""
echo "Starting makepkg..."
echo "────────────────────────────────────────────────────────────────────"

# pacman.conf has NoProgressBar set, so download progress shows as text
makepkg -s --noconfirm --needed

echo "────────────────────────────────────────────────────────────────────"
echo ""
echo "╔════════════════════════════════════════════════════════════════╗"
echo "║  BUILD COMPLETE                                                ║"
echo "╚════════════════════════════════════════════════════════════════╝"
echo ""
echo "Packages produced:"
for f in /pkgdest/*.pkg.tar.*; do
    [ -e "$f" ] || continue
    size=$(du -h "$f" | cut -f1)
    name=$(basename "$f")
    echo "  ✓ $name ($size)"
done
echo ""
"#;

/// Stream lines from a reader, splitting on both \n and \r so that progress bars
/// (curl, wget, etc.) that use \r to overwrite the current line display in real time.
/// Lines from \r are printed with \r to allow overwriting on the terminal.
fn stream_lines<R: std::io::Read>(reader: std::io::BufReader<R>, log: &mut std::fs::File, is_stderr: bool) {
    use std::io::Read;
    let mut buf = Vec::with_capacity(4096);
    for byte in reader.bytes() {
        match byte {
            Ok(b'\n') => {
                let line = String::from_utf8_lossy(&buf);
                if is_stderr {
                    eprintln!("{}", line);
                    let _ = writeln!(log, "[stderr] {}", line);
                } else {
                    println!("{}", line);
                    let _ = writeln!(log, "{}", line);
                }
                buf.clear();
            }
            Ok(b'\r') => {
                let line = String::from_utf8_lossy(&buf);
                if !line.is_empty() {
                    if is_stderr {
                        eprint!("\r{}", line);
                    } else {
                        print!("\r{}", line);
                    }
                    // Don't log every \r progress update to avoid log bloat
                }
                buf.clear();
            }
            Ok(b) => buf.push(b),
            Err(_) => break,
        }
    }
    // Flush any remaining content
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        if is_stderr {
            eprintln!("{}", line);
            let _ = writeln!(log, "[stderr] {}", line);
        } else {
            println!("{}", line);
            let _ = writeln!(log, "{}", line);
        }
    }
}

/// Compare two version strings using pacman's vercmp
/// Returns Ordering::Greater if ver1 > ver2, etc.
/// Falls back to lexicographic comparison if vercmp is not available
pub fn vercmp(ver1: &str, ver2: &str) -> Ordering {
    let output = Command::new("vercmp")
        .args([ver1, ver2])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let result = String::from_utf8_lossy(&out.stdout)
                .trim()
                .parse::<i32>()
                .unwrap_or(0);
            result.cmp(&0)
        }
        _ => {
            // Fallback to lexicographic comparison
            tracing::warn!(
                "vercmp not found or failed; falling back to lexicographic comparison \
                 (version comparison may be incorrect)"
            );
            ver1.cmp(ver2)
        }
    }
}

/// Build system using Podman containers
pub struct BuildSystem {
    config: Config,
    cache: ResolvedCacheConfig,
}

impl BuildSystem {
    pub fn new(config: Config) -> Self {
        let cache = config.resolved_cache();
        Self { config, cache }
    }

    /// Check if Podman is available
    pub fn check_podman() -> Result<()> {
        let output = Command::new("podman")
            .arg("--version")
            .output()
            .context("Failed to execute podman")?;

        if !output.status.success() {
            anyhow::bail!("Podman not available");
        }

        Ok(())
    }

    /// Check if the base image exists
    pub fn image_exists(&self) -> Result<bool> {
        let output = Command::new("podman")
            .args(["image", "exists", &self.config.container.image])
            .output()
            .context("Failed to check image existence")?;

        Ok(output.status.success())
    }

    /// Build the base container image
    /// Tags with both the base name and a date-versioned tag
    pub fn build_base_image(&self) -> Result<()> {
        let dockerfile = self.generate_dockerfile();

        // Write Dockerfile to temp location
        let dockerfile_path = self.cache.logs.join("Dockerfile.repoman");
        std::fs::write(&dockerfile_path, &dockerfile)
            .context("Failed to write Dockerfile")?;

        // Generate date tag (YYYY-MM-DD)
        let date_tag = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let dated_image = format!("{}:{}", self.config.container.image, date_tag);

        println!("Building base image: {} ({})", self.config.container.image, dated_image);

        // Build with date tag
        let status = Command::new("podman")
            .args([
                "build",
                "--no-cache",
                "-t",
                &dated_image,
                "-f",
                &dockerfile_path.to_string_lossy(),
                &self.cache.logs.to_string_lossy(),
            ])
            .stdin(Stdio::null())
            .status()
            .context("Failed to build base image")?;

        if !status.success() {
            anyhow::bail!("Failed to build base image");
        }

        // Also tag as the base name (latest)
        let status = Command::new("podman")
            .args(["tag", &dated_image, &self.config.container.image])
            .status()
            .context("Failed to tag base image")?;

        if !status.success() {
            anyhow::bail!("Failed to tag base image as latest");
        }

        println!("Image built and tagged: {}, {}", dated_image, self.config.container.image);
        Ok(())
    }

    /// Generate Dockerfile for base image
    /// Note: The local repo is configured at container startup via REPOMAN_REPO_NAME env var,
    /// so the image doesn't need to be rebuilt when the repo name changes.
    fn generate_dockerfile(&self) -> String {
        r#"FROM archlinux:base-devel

# Update system and install required packages
RUN pacman -Syu --noconfirm && \
    pacman -S --noconfirm --needed \
        git \
        fakeroot \
        binutils \
        debugedit \
        sudo

# Create build user with proper permissions
RUN useradd -m -s /bin/bash builder && \
    echo "builder ALL=(ALL) NOPASSWD: ALL" >> /etc/sudoers.d/builder && \
    chmod 440 /etc/sudoers.d/builder

# Initialize pacman keyring
RUN pacman-key --init && \
    pacman-key --populate archlinux

# Configure makepkg for parallel builds and output location
RUN sed -i 's/^#MAKEFLAGS=.*/MAKEFLAGS="-j$(nproc)"/' /etc/makepkg.conf && \
    sed -i 's|^#PKGDEST=.*|PKGDEST=/pkgdest|' /etc/makepkg.conf && \
    sed -i 's|^#SRCDEST=.*|SRCDEST=/build/sources|' /etc/makepkg.conf

# Configure pacman for non-interactive use (NoProgressBar shows text output instead of terminal progress bars)
RUN sed -i '/^\[options\]/a NoProgressBar' /etc/pacman.conf && \
    sed -i '/^\[options\]/a VerbosePkgLists' /etc/pacman.conf

# Create directories
RUN mkdir -p /build /build/sources /pkgdest /repo && \
    chown -R builder:builder /build /pkgdest

USER builder
WORKDIR /build

# Default command: sync repos and build
CMD ["bash", "-c", "sudo pacman -Sy --noconfirm && makepkg -s --noconfirm --needed"]
"#
        .to_string()
    }

    /// Run a build for a package (synchronous version for CLI)
    pub fn run_build_sync(
        &self,
        pkgbase: &str,
        source_path: &Path,
        log_path: &Path,
    ) -> Result<Vec<PathBuf>> {
        let container_name = format!("repoman-build-{}", pkgbase);

        // Clean up any previous pkgdest (may have permission issues from container subuids)
        self.cleanup_pkgdest(pkgbase)?;

        // Create output directory for packages
        let pkgdest = self.cache.logs.join(format!("pkgdest-{}", pkgbase));
        std::fs::create_dir_all(&pkgdest)?;

        // Create sources directory (SRCDEST) for this package
        let sources_dir = self.sources_dir(pkgbase);
        std::fs::create_dir_all(&sources_dir)?;

        // Prepare mount paths
        // :z = shared SELinux label
        // :U = chown to match container user (needed for rootless podman)
        let source_mount = format!("{}:/build/src:z", source_path.display());
        let repo_mount = format!("{}:/repo:z", self.config.repo.path.display());
        let cache_mount = format!(
            "{}:/var/cache/pacman/pkg:z,U",
            self.cache.pacman_cache.display()
        );
        let pkgdest_mount = format!("{}:/pkgdest:z,U", pkgdest.display());
        let srcdest_mount = format!("{}:/build/sources:z,U", sources_dir.display());

        // Open log file
        let log_file = std::fs::File::create(log_path)
            .with_context(|| format!("Failed to create log file: {}", log_path.display()))?;

        println!("Starting build for {} in container {}", pkgbase, container_name);
        println!("Logs: {}", log_path.display());

        // Build podman args dynamically
        let mut podman_args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            container_name.clone(),
            "-v".to_string(),
            source_mount,
            "-v".to_string(),
            repo_mount,
            "-v".to_string(),
            cache_mount,
            "-v".to_string(),
            pkgdest_mount,
            "-v".to_string(),
            srcdest_mount,
        ];

        // Add tmpfs mount for build directory if configured
        if self.config.container.tmpfs_build {
            let tmpfs_opts = match &self.config.container.tmpfs_size {
                Some(size) => format!("type=tmpfs,destination=/build/work,tmpfs-size={}", size),
                None => "type=tmpfs,destination=/build/work".to_string(),
            };
            podman_args.push("--mount".to_string());
            podman_args.push(tmpfs_opts);
            println!("Using tmpfs for build directory");
        }

        // Pass repo name as env var for runtime pacman.conf configuration
        podman_args.push("-e".to_string());
        podman_args.push(format!("REPOMAN_REPO_NAME={}", self.config.repo.name));

        podman_args.push(self.config.container.image.clone());
        podman_args.push("bash".to_string());
        podman_args.push("-c".to_string());
        podman_args.push(BUILD_SCRIPT.to_string());

        let mut child = Command::new("podman")
            .args(&podman_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to start build container")?;

        // Stream output to both console and log file
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let log_file_clone = log_file.try_clone()?;

        // Spawn threads to handle output
        // Uses stream_lines which splits on both \r and \n so that curl/wget
        // progress bars (which use \r to overwrite the line) display in real time.
        let stdout_handle = std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            let mut log = log_file;
            stream_lines(reader, &mut log, false);
        });

        let stderr_handle = std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            let mut log = log_file_clone;
            stream_lines(reader, &mut log, true);
        });

        let status = child.wait().context("Failed to wait for build")?;

        stdout_handle.join().map_err(|_| anyhow::anyhow!("stdout thread panicked"))?;
        stderr_handle.join().map_err(|_| anyhow::anyhow!("stderr thread panicked"))?;

        if !status.success() {
            anyhow::bail!("Build failed with exit code: {:?}", status.code());
        }

        // Find built packages in pkgdest
        let packages = self.find_built_packages(&pkgdest)?;

        if packages.is_empty() {
            anyhow::bail!("Build succeeded but no packages were produced");
        }

        println!("Built {} package(s):", packages.len());
        for pkg in &packages {
            println!("  - {}", pkg.file_name().unwrap_or_default().to_string_lossy());
        }

        Ok(packages)
    }

    /// Find built package files in a directory
    pub fn find_built_packages(&self, pkgdest: &Path) -> Result<Vec<PathBuf>> {
        let mut packages = Vec::new();

        for entry in std::fs::read_dir(pkgdest)? {
            let entry = entry?;
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz") {
                    packages.push(path);
                }
            }
        }

        Ok(packages)
    }

    /// Move built packages to repository
    pub fn move_to_repo(&self, packages: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let mut repo_packages = Vec::new();

        for (i, pkg) in packages.iter().enumerate() {
            let filename = pkg
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("Invalid package path"))?;
            let dest = self.config.repo.path.join(filename);

            let size = std::fs::metadata(pkg)
                .map(|m| m.len())
                .unwrap_or(0);
            let size_str = format_size(size);

            tracing::info!(
                "[{}/{}] Copying {} ({}) -> {}",
                i + 1,
                packages.len(),
                pkg.display(),
                size_str,
                dest.display(),
            );

            // Copy with progress for large files (>100MB)
            if size > 100 * 1024 * 1024 {
                copy_with_progress(pkg, &dest, size)
                    .with_context(|| format!("Failed to copy package to repo: {}", pkg.display()))?;
            } else {
                std::fs::copy(pkg, &dest)
                    .with_context(|| format!("Failed to copy package to repo: {}", pkg.display()))?;
            }

            tracing::info!(
                "[{}/{}] Copied {} to repo",
                i + 1,
                packages.len(),
                filename.to_string_lossy(),
            );

            // Try to delete original, but don't fail if we can't (permission issues with container subuids)
            if let Err(e) = std::fs::remove_file(pkg) {
                tracing::warn!("Could not remove original package {}: {} (will be cleaned up later)", pkg.display(), e);
            }

            repo_packages.push(dest);
        }

        Ok(repo_packages)
    }

    /// Clean up pkgdest directory using podman unshare (handles subuid permissions)
    pub fn cleanup_pkgdest(&self, pkgbase: &str) -> Result<()> {
        let pkgdest = self.cache.logs.join(format!("pkgdest-{}", pkgbase));
        if pkgdest.exists() {
            let status = Command::new("podman")
                .args(["unshare", "rm", "-rf", &pkgdest.to_string_lossy()])
                .status();

            match status {
                Ok(s) if s.success() => {}
                _ => {
                    // Fallback to regular rm
                    let _ = std::fs::remove_dir_all(&pkgdest);
                }
            }
        }
        Ok(())
    }

    /// Get the sources directory for a package (SRCDEST)
    pub fn sources_dir(&self, pkgbase: &str) -> PathBuf {
        self.cache.sources.join(pkgbase)
    }

    /// Clean up stale sources when SRCINFO changes
    /// Compares old and new SRCINFO, deletes sources whose checksums changed
    /// Returns list of deleted files
    pub fn cleanup_stale_sources(
        &self,
        pkgbase: &str,
        old_srcinfo: Option<&SrcInfo>,
        new_srcinfo: &SrcInfo,
    ) -> Result<Vec<String>> {
        let sources_dir = self.sources_dir(pkgbase);
        if !sources_dir.exists() {
            return Ok(vec![]);
        }

        // Build map of filename -> checksum for new sources
        let new_sources: HashMap<String, Option<String>> = new_srcinfo
            .sources_with_checksums()
            .into_iter()
            .collect();

        // If no old srcinfo, check if version changed and clean all
        let old_sources: HashMap<String, Option<String>> = old_srcinfo
            .map(|s| s.sources_with_checksums().into_iter().collect())
            .unwrap_or_default();

        let mut deleted = Vec::new();

        // If we have old srcinfo, do selective cleanup
        if old_srcinfo.is_some() {
            for (filename, old_checksum) in &old_sources {
                let should_delete = match new_sources.get(filename) {
                    // File no longer in sources - delete
                    None => true,
                    // File still in sources - check if checksum changed
                    Some(new_checksum) => {
                        // If both have checksums, compare them
                        // If either is None (SKIP or no checksums), keep the file
                        match (old_checksum, new_checksum) {
                            (Some(old), Some(new)) => old != new,
                            _ => false, // Keep if we can't verify
                        }
                    }
                };

                if should_delete {
                    let file_path = sources_dir.join(filename);
                    if file_path.exists() {
                        // Use podman unshare for permission handling
                        let status = Command::new("podman")
                            .args(["unshare", "rm", "-f", &file_path.to_string_lossy()])
                            .status();

                        match status {
                            Ok(s) if s.success() => {
                                deleted.push(filename.clone());
                            }
                            _ => {
                                // Fallback to regular rm
                                if std::fs::remove_file(&file_path).is_ok() {
                                    deleted.push(filename.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(deleted)
    }

    /// Clean all sources for a package
    pub fn cleanup_all_sources(&self, pkgbase: &str) -> Result<()> {
        let sources_dir = self.sources_dir(pkgbase);
        if sources_dir.exists() {
            let status = Command::new("podman")
                .args(["unshare", "rm", "-rf", &sources_dir.to_string_lossy()])
                .status();

            match status {
                Ok(s) if s.success() => {}
                _ => {
                    let _ = std::fs::remove_dir_all(&sources_dir);
                }
            }
        }
        Ok(())
    }

    /// Initialize an empty repository database if it doesn't exist
    pub fn init_repo(&self) -> Result<()> {
        let db_path = self
            .config
            .repo
            .path
            .join(format!("{}.db.tar.gz", self.config.repo.name));

        if db_path.exists() {
            return Ok(());
        }

        println!("Initializing empty repository database: {}", db_path.display());

        // Create an empty tar.gz file as the repo database
        // repo-add will create a proper one when we add the first package
        // For now, we just need a valid (empty) database
        let output = Command::new("bsdtar")
            .args(["-czf", &db_path.to_string_lossy(), "-T", "/dev/null"])
            .output()
            .context("Failed to create empty repo database")?;

        if !output.status.success() {
            // Try with tar if bsdtar isn't available
            let output = Command::new("tar")
                .args(["czf", &db_path.to_string_lossy(), "--files-from=/dev/null"])
                .output()
                .context("Failed to create empty repo database")?;

            if !output.status.success() {
                anyhow::bail!(
                    "Failed to create empty repo database: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        // Also create the symlink that pacman expects
        let db_link = self
            .config
            .repo
            .path
            .join(format!("{}.db", self.config.repo.name));
        match std::os::unix::fs::symlink(
            format!("{}.db.tar.gz", self.config.repo.name),
            &db_link,
        ) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }

        Ok(())
    }

    /// Add packages to repository database
    pub fn repo_add(&self, packages: &[PathBuf]) -> Result<()> {
        if packages.is_empty() {
            return Ok(());
        }

        let db_path = self
            .config
            .repo
            .path
            .join(format!("{}.db.tar.gz", self.config.repo.name));

        println!("Adding packages to repository database: {}", db_path.display());

        let mut cmd = Command::new("repo-add");
        cmd.arg(&db_path);

        for pkg in packages {
            cmd.arg(pkg);
        }

        let output = cmd.output().context("Failed to execute repo-add")?;

        if !output.status.success() {
            anyhow::bail!(
                "repo-add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        println!("Repository database updated successfully");
        Ok(())
    }

    /// Remove packages from repository database
    pub fn repo_remove(&self, pkgnames: &[&str]) -> Result<()> {
        if pkgnames.is_empty() {
            return Ok(());
        }

        let db_path = self
            .config
            .repo
            .path
            .join(format!("{}.db.tar.gz", self.config.repo.name));

        // Check if db exists first
        if !db_path.exists() {
            return Ok(());
        }

        let mut cmd = Command::new("repo-remove");
        cmd.arg(&db_path);

        for name in pkgnames {
            cmd.arg(name);
        }

        let output = cmd.output().context("Failed to execute repo-remove")?;

        if !output.status.success() {
            // repo-remove fails if package isn't in db, which is ok
            tracing::warn!(
                "repo-remove warning: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// Stop a running build container
    pub fn stop_build(&self, container_name: &str) -> Result<()> {
        let output = Command::new("podman")
            .args(["stop", "-t", "5", container_name])
            .output()
            .context("Failed to stop container")?;

        if !output.status.success() {
            tracing::warn!(
                "Failed to stop container {}: {}",
                container_name,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    /// List repoman build containers (running or stopped)
    pub fn list_containers(&self) -> Result<Vec<ContainerInfo>> {
        let output = Command::new("podman")
            .args([
                "ps", "-a",
                "--filter", "name=repoman-build-",
                "--format", "{{.ID}}\t{{.Names}}\t{{.Status}}\t{{.CreatedAt}}",
            ])
            .output()
            .context("Failed to list containers")?;

        if !output.status.success() {
            anyhow::bail!(
                "podman ps failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let containers = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 4 {
                    Some(ContainerInfo {
                        id: parts[0].to_string(),
                        name: parts[1].to_string(),
                        status: parts[2].to_string(),
                        created: parts[3].to_string(),
                        running: parts[2].to_lowercase().contains("up"),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(containers)
    }

    /// Remove stopped repoman build containers
    pub fn prune_containers(&self) -> Result<PruneResult> {
        let containers = self.list_containers()?;
        let mut result = PruneResult {
            removed: Vec::new(),
            errors: Vec::new(),
        };

        for container in containers {
            if container.running {
                continue; // Don't remove running containers
            }

            let output = Command::new("podman")
                .args(["rm", "-f", &container.id])
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    result.removed.push(container.name);
                }
                Ok(o) => {
                    result.errors.push(format!(
                        "{}: {}",
                        container.name,
                        String::from_utf8_lossy(&o.stderr).trim()
                    ));
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", container.name, e));
                }
            }
        }

        Ok(result)
    }

    /// List repoman builder images
    pub fn list_images(&self) -> Result<Vec<ImageInfo>> {
        // Get the base image name without any tag
        let base_name = &self.config.container.image;

        let output = Command::new("podman")
            .args([
                "images",
                "--format", "{{.ID}}\t{{.Repository}}\t{{.Tag}}\t{{.CreatedAt}}\t{{.Size}}",
                base_name,
            ])
            .output()
            .context("Failed to list images")?;

        if !output.status.success() {
            // No images found is not an error
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let images = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 5 {
                    Some(ImageInfo {
                        id: parts[0].to_string(),
                        repository: parts[1].to_string(),
                        tag: parts[2].to_string(),
                        created: parts[3].to_string(),
                        size: parts[4].to_string(),
                    })
                } else {
                    None
                }
            })
            .collect();

        Ok(images)
    }

    /// Prune old repoman builder images, keeping the N most recent
    pub fn prune_images(&self, keep: usize) -> Result<PruneResult> {
        let mut images = self.list_images()?;
        let mut result = PruneResult {
            removed: Vec::new(),
            errors: Vec::new(),
        };

        if images.len() <= keep {
            return Ok(result); // Nothing to prune
        }

        // Sort by tag (date tags sort chronologically as YYYY-MM-DD)
        // Put "latest" and non-date tags at the beginning to preserve them
        images.sort_by(|a, b| {
            let a_is_date = a.tag.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
            let b_is_date = b.tag.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
            match (a_is_date, b_is_date) {
                (false, true) => Ordering::Less,    // non-date tags first
                (true, false) => Ordering::Greater,
                _ => b.tag.cmp(&a.tag), // newest dates first
            }
        });

        // Keep the first N images (which includes "latest" + newest dated)
        // But we need to be careful: "latest" often points to the same image as a dated tag
        // So we track image IDs we're keeping
        let mut kept_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut kept_count = 0;

        for image in &images {
            if kept_count < keep {
                if kept_ids.insert(image.id.clone()) {
                    kept_count += 1;
                }
            }
        }

        // Remove images not in kept set
        for image in &images {
            if kept_ids.contains(&image.id) {
                continue;
            }

            let image_ref = format!("{}:{}", image.repository, image.tag);
            let output = Command::new("podman")
                .args(["rmi", &image_ref])
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    result.removed.push(image_ref);
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    // Ignore "image is in use" errors
                    if !stderr.contains("image is being used") {
                        result.errors.push(format!("{}: {}", image_ref, stderr.trim()));
                    }
                }
                Err(e) => {
                    result.errors.push(format!("{}: {}", image_ref, e));
                }
            }
        }

        Ok(result)
    }
}

/// Information about a container
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub status: String,
    pub created: String,
    pub running: bool,
}

/// Information about an image
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageInfo {
    pub id: String,
    pub repository: String,
    pub tag: String,
    pub created: String,
    pub size: String,
}

/// Result of a prune operation
#[derive(Debug, Clone, serde::Serialize)]
pub struct PruneResult {
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn copy_with_progress(src: &Path, dest: &Path, total: u64) -> Result<()> {
    let mut reader = std::fs::File::open(src)?;
    let mut writer = std::fs::File::create(dest)?;

    let mut buf = vec![0u8; 8 * 1024 * 1024]; // 8 MiB buffer
    let mut copied: u64 = 0;
    let mut last_log_pct: u64 = 0;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut writer, &buf[..n])?;
        copied += n as u64;

        if total > 0 {
            let pct = copied * 100 / total;
            // Log every 10%
            if pct >= last_log_pct + 10 {
                last_log_pct = pct / 10 * 10;
                tracing::info!(
                    "  copy progress: {} / {} ({}%)",
                    format_size(copied),
                    format_size(total),
                    last_log_pct,
                );
            }
        }
    }

    Ok(())
}

