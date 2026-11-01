//! cgroup v2 management for per-job limits and peak accounting.
//!
//! On start the worker moves itself into a `supervisor` child of its own
//! cgroup so that controllers can be enabled for sibling job cgroups (the
//! "no internal processes" rule). When that is not possible, for example
//! running unprivileged, limits and peaks are simply unavailable and the
//! worker says so once.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use repoman_common::proto::Peaks;

pub struct CgroupManager {
    base: Option<PathBuf>,
}

pub struct JobCgroup {
    pub path: PathBuf,
}

const CONTROLLERS: &[&str] = &["memory", "cpu", "pids"];

impl CgroupManager {
    pub fn disabled() -> Self {
        CgroupManager { base: None }
    }

    /// Try to set up delegation under our own cgroup. Never fails; logs why
    /// cgroups are unavailable instead.
    pub fn init() -> Self {
        match Self::try_init() {
            Ok(base) => {
                tracing::info!(base = %base.display(), "job cgroups enabled");
                CgroupManager { base: Some(base) }
            }
            Err(e) => {
                tracing::warn!(error = %e, "job cgroups unavailable; no memory limits or peak accounting");
                CgroupManager { base: None }
            }
        }
    }

    fn try_init() -> Result<PathBuf> {
        let own = own_cgroup()?;
        let own = Path::new("/sys/fs/cgroup").join(own.trim_start_matches('/'));
        anyhow::ensure!(own.is_dir(), "cgroup dir {} missing", own.display());
        // systemd's DelegateSubgroup=supervisor starts us already inside the
        // leaf; the delegated base (where controllers can be enabled for job
        // cgroups) is then our parent. Started elsewhere, we are in the base
        // and create the leaf ourselves.
        let (base, supervisor, already_in_leaf) = match own.file_name().and_then(|n| n.to_str()) {
            Some("supervisor") => (own.parent().context("supervisor has no parent")?.to_path_buf(), own.clone(), true),
            _ => (own.clone(), own.join("supervisor"), false),
        };
        let available = std::fs::read_to_string(base.join("cgroup.controllers"))
            .context("reading cgroup.controllers")?;
        let wanted: Vec<&str> = CONTROLLERS
            .iter()
            .copied()
            .filter(|c| available.split_whitespace().any(|a| a == *c))
            .collect();
        anyhow::ensure!(wanted.contains(&"memory"), "memory controller not delegated to {}", base.display());

        if !already_in_leaf {
            // Move ourselves out of the way.
            std::fs::create_dir_all(&supervisor)?;
            std::fs::write(supervisor.join("cgroup.procs"), std::process::id().to_string())
                .context("moving worker into supervisor cgroup")?;
        }
        // Any leftover processes in the base (none expected) would block this.
        let enable = wanted.iter().map(|c| format!("+{c}")).collect::<Vec<_>>().join(" ");
        std::fs::write(base.join("cgroup.subtree_control"), enable)
            .context("enabling controllers for job cgroups")?;
        Ok(base)
    }

    pub fn enabled(&self) -> bool {
        self.base.is_some()
    }

    pub fn create(&self, job_id: &str, memory_limit: Option<u64>) -> Option<JobCgroup> {
        let base = self.base.as_ref()?;
        let path = base.join(format!("job-{job_id}"));
        if let Err(e) = std::fs::create_dir_all(&path) {
            tracing::warn!(error = %e, "creating job cgroup");
            return None;
        }
        // Kill the whole job if any part of it hits the limit.
        let _ = std::fs::write(path.join("memory.oom.group"), "1");
        if let Some(limit) = memory_limit {
            if let Err(e) = std::fs::write(path.join("memory.max"), limit.to_string()) {
                tracing::warn!(error = %e, "setting memory.max");
            }
        }
        Some(JobCgroup { path })
    }
}

impl JobCgroup {
    pub fn peaks(&self) -> Peaks {
        let mut p = Peaks::default();
        if let Ok(s) = std::fs::read_to_string(self.path.join("memory.peak")) {
            p.peak_rss_bytes = s.trim().parse().ok();
        }
        if let Ok(s) = std::fs::read_to_string(self.path.join("cpu.stat")) {
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("usage_usec ") {
                    p.cpu_seconds = v.trim().parse::<f64>().ok().map(|us| us / 1e6);
                }
            }
        }
        p
    }

    /// Was the job killed by the memory controller?
    pub fn oom_killed(&self) -> bool {
        std::fs::read_to_string(self.path.join("memory.events"))
            .map(|s| {
                s.lines().any(|l| {
                    l.strip_prefix("oom_kill ")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .is_some_and(|n| n > 0)
                })
            })
            .unwrap_or(false)
    }

    /// Kill anything left and remove the cgroup.
    pub fn cleanup(self) {
        let _ = std::fs::write(self.path.join("cgroup.kill"), "1");
        for _ in 0..50 {
            if std::fs::remove_dir(&self.path).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        tracing::warn!(path = %self.path.display(), "could not remove job cgroup");
    }
}

fn own_cgroup() -> Result<String> {
    let s = std::fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            return Ok(rest.to_string());
        }
    }
    anyhow::bail!("no cgroup v2 entry in /proc/self/cgroup")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_parse_from_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("memory.peak"), "123456\n").unwrap();
        std::fs::write(d.path().join("cpu.stat"), "usage_usec 2500000\nuser_usec 1\n").unwrap();
        std::fs::write(d.path().join("memory.events"), "low 0\noom_kill 1\n").unwrap();
        let cg = JobCgroup {
            path: d.path().to_path_buf(),
        };
        let p = cg.peaks();
        assert_eq!(p.peak_rss_bytes, Some(123456));
        assert_eq!(p.cpu_seconds, Some(2.5));
        assert!(cg.oom_killed());
    }

    #[test]
    fn own_cgroup_is_readable() {
        let c = own_cgroup().unwrap();
        assert!(c.starts_with('/'));
    }
}
