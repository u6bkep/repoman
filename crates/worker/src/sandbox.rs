//! Isolated command execution: a fresh mount, PID, UTS, and IPC namespace
//! (optionally a network namespace with only loopback), an overlayfs root
//! over an immutable image, bind mounts for the job directory, source cache,
//! and package cache, then `chroot`. Everything is driven through util-linux
//! `unshare`, so mounts vanish when the command exits and teardown is just
//! deleting the upper directory.
//!
//! Requires root. The build phases run as an unprivileged uid inside the
//! root through `setpriv`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

/// What backs the root filesystem.
#[derive(Debug, Clone)]
pub enum RootSource {
    /// overlayfs: `lower` is the image, `upper` and `work` are per job.
    Overlay {
        lower: PathBuf,
        upper: PathBuf,
        work: PathBuf,
    },
    /// Bind-mount a directory directly; used while constructing an image.
    Dir(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    pub root: RootSource,
    /// Mount point for the assembled root, per job.
    pub mountpoint: PathBuf,
    /// Host path → path inside the root. Created if missing.
    pub binds: Vec<(PathBuf, String)>,
    pub network: bool,
    /// cgroup directory to join before anything else, if any.
    pub cgroup: Option<PathBuf>,
    pub hostname: String,
    /// `chroot` into the assembled root before exec (normal), or run the
    /// command on the host side with the root mounted at `mountpoint`
    /// (bootstrapping an image with the host pacman).
    pub chroot: bool,
    /// Offline sandboxes only: route all traffic into a sink interface with
    /// an nftables counter and write the counter (nft JSON) here when the
    /// command exits, so the worker can report network attempts.
    pub netlog: Option<PathBuf>,
}

/// Who runs the command inside the root.
#[derive(Debug, Clone, Copy)]
pub enum RunAs {
    Root,
    User { uid: u32, gid: u32 },
}

pub const SETUP_SCRIPT: &str = r#"set -eu
if [ -n "${REPOMAN_CGROUP:-}" ]; then echo $$ > "$REPOMAN_CGROUP/cgroup.procs"; fi
ROOT="$REPOMAN_ROOT"
mkdir -p "$ROOT"
if [ -n "${REPOMAN_UPPER:-}" ]; then
    # The previous phase's namespace may still be tearing down (processes
  # killed with the PID namespace exit asynchronously), which makes the
  # workdir briefly busy. Retry for up to ten seconds.
  i=0
  while :; do
    if err=$(mount -t overlay overlay -o "lowerdir=$REPOMAN_LOWER,upperdir=$REPOMAN_UPPER,workdir=$REPOMAN_WORK" "$ROOT" 2>&1); then break; fi
    i=$((i+1))
    if [ "$i" -ge 100 ]; then echo "$err" >&2; exit 32; fi
    sleep 0.1
  done
else
  mount --bind "$REPOMAN_LOWER" "$ROOT"
fi
mount -t proc -o nosuid,noexec,nodev proc "$ROOT/proc"
mount -t sysfs -o ro,nosuid,nodev,noexec sys "$ROOT/sys" 2>/dev/null || { mount --rbind /sys "$ROOT/sys"; mount --make-rslave "$ROOT/sys"; }
mount -t tmpfs -o mode=755,nosuid,nodev tmpfs "$ROOT/run"
mount -t tmpfs -o nosuid,nodev tmpfs "$ROOT/tmp"
mkdir -p "$ROOT/dev"
mount --rbind /dev "$ROOT/dev"
mount --make-rslave "$ROOT/dev"
i=0
while [ $i -lt "${REPOMAN_BINDS:-0}" ]; do
  eval "src=\$REPOMAN_BIND_SRC_$i"; eval "dst=\$REPOMAN_BIND_DST_$i"
  mkdir -p "$ROOT$dst"
  mount --bind "$src" "$ROOT$dst"
  i=$((i+1))
done
hostname "${REPOMAN_HOSTNAME:-repoman}" 2>/dev/null || true
NETCOUNT=0
if [ "${REPOMAN_NET:-0}" = 1 ]; then
  cp -L /etc/resolv.conf "$ROOT/etc/resolv.conf" 2>/dev/null || true
else
  ip link set lo up 2>/dev/null || true
  : > "$ROOT/etc/resolv.conf"
  # Count attempts: a sink interface takes the default route, nftables
  # counts and rejects what reaches it (reject, not drop, so connect()
  # fails at once instead of waiting out TCP retries), and a fake
  # nameserver gives DNS lookups somewhere to go.
  if [ -n "${REPOMAN_NETLOG:-}" ] && command -v nft >/dev/null 2>&1 && ip link add repoman0 type dummy 2>/dev/null; then
    ip addr add 10.255.255.1/32 dev repoman0 && ip link set repoman0 up       && ip route add default dev repoman0       && { ip -6 route add default dev repoman0 2>/dev/null || true; }       && printf 'table inet repoman {\n counter attempts {}\n chain out { type filter hook output priority 0; policy accept; oifname "repoman0" counter name "attempts" reject }\n}\n' | nft -f -       && printf 'nameserver 10.255.255.254\noptions timeout:1 attempts:1\n' > "$ROOT/etc/resolv.conf"       && NETCOUNT=1
  fi
fi
if [ "${REPOMAN_CHROOT:-1}" = 1 ]; then
  if [ "$NETCOUNT" = 1 ]; then
    set +e
    chroot "$ROOT" /bin/sh -c 'cd "$1" && shift && exec "$@"' sh "${REPOMAN_CWD:-/}" "$@"
    st=$?
    nft -j list counter inet repoman attempts > "$REPOMAN_NETLOG" 2>/dev/null || true
    exit $st
  fi
  exec chroot "$ROOT" /bin/sh -c 'cd "$1" && shift && exec "$@"' sh "${REPOMAN_CWD:-/}" "$@"
fi
cd "${REPOMAN_CWD:-/}"
exec "$@"
"#;

impl Sandbox {
    /// Build the host-side command. `argv` runs inside the root; `cwd` is a
    /// path inside the root; `env` is applied inside the root.
    pub fn command(&self, argv: &[String], run_as: RunAs, cwd: &str, env: &[(String, String)]) -> Result<Command> {
        anyhow::ensure!(!argv.is_empty(), "sandbox command needs an argv");
        let mut cmd = Command::new("unshare");
        cmd.arg("--mount")
            .arg("--uts")
            .arg("--ipc")
            .arg("--pid")
            .arg("--fork")
            .arg("--kill-child")
            .arg("--propagation")
            .arg("private");
        if !self.network {
            cmd.arg("--net");
        }
        cmd.arg("--").arg("/bin/sh").arg("-c").arg(SETUP_SCRIPT).arg("repoman-sandbox");

        // Inside the root: fix up environment and identity, then exec argv.
        let path_var = "/usr/local/sbin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin";
        let mut inner: Vec<String> = Vec::new();
        match run_as {
            RunAs::Root => {
                inner.push("env".into());
                inner.push("-i".into());
            }
            RunAs::User { uid, gid } => {
                inner.extend([
                    "setpriv".to_string(),
                    format!("--reuid={uid}"),
                    format!("--regid={gid}"),
                    "--clear-groups".into(),
                    "--reset-env".into(),
                    "env".into(),
                ]);
            }
        }
        inner.push(format!("PATH={path_var}"));
        inner.push("LANG=C.UTF-8".into());
        inner.push("LC_ALL=C.UTF-8".into());
        inner.push("TERM=dumb".into());
        for (k, v) in env {
            inner.push(format!("{k}={v}"));
        }
        inner.extend(argv.iter().cloned());
        cmd.args(&inner);

        match &self.root {
            RootSource::Overlay { lower, upper, work } => {
                cmd.env("REPOMAN_LOWER", lower)
                    .env("REPOMAN_UPPER", upper)
                    .env("REPOMAN_WORK", work);
            }
            RootSource::Dir(d) => {
                cmd.env("REPOMAN_LOWER", d).env_remove("REPOMAN_UPPER");
            }
        }
        cmd.env("REPOMAN_ROOT", &self.mountpoint)
            .env("REPOMAN_CWD", cwd)
            .env("REPOMAN_CHROOT", if self.chroot { "1" } else { "0" })
            .env("REPOMAN_NET", if self.network { "1" } else { "0" })
            .env("REPOMAN_HOSTNAME", &self.hostname)
            .env("REPOMAN_BINDS", self.binds.len().to_string());
        for (i, (src, dst)) in self.binds.iter().enumerate() {
            cmd.env(format!("REPOMAN_BIND_SRC_{i}"), src)
                .env(format!("REPOMAN_BIND_DST_{i}"), dst);
        }
        if let Some(cg) = &self.cgroup {
            cmd.env("REPOMAN_CGROUP", cg);
        }
        if let Some(n) = &self.netlog
            && !self.network
        {
            cmd.env("REPOMAN_NETLOG", n);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        Ok(cmd)
    }

    /// Prepare directories the setup script expects to exist.
    pub fn prepare_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.mountpoint)
            .with_context(|| format!("creating {}", self.mountpoint.display()))?;
        if let RootSource::Overlay { upper, work, .. } = &self.root {
            std::fs::create_dir_all(upper)?;
            std::fs::create_dir_all(work)?;
        }
        for (src, _) in &self.binds {
            std::fs::create_dir_all(src)?;
        }
        Ok(())
    }
}

/// Is this process able to run sandboxes: root on Linux with the tools present.
pub fn available() -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        return Err("not linux".into());
    }
    if !is_root() {
        return Err("not running as root".into());
    }
    for tool in ["unshare", "chroot", "setpriv", "mount", "ip", "bsdtar", "zstd"] {
        if which(tool).is_none() {
            return Err(format!("missing tool: {tool}"));
        }
    }
    Ok(())
}

pub fn is_root() -> bool {
    // Avoid a libc dependency for one call.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(|u| u == "0"))
        })
        .unwrap_or(false)
}

pub fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(tool);
        if p.is_file() {
            return Some(p);
        }
    }
    for dir in ["/usr/bin", "/usr/sbin", "/bin", "/sbin"] {
        let p = Path::new(dir).join(tool);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_shape_for_overlay_user_offline() {
        let sb = Sandbox {
            root: RootSource::Overlay {
                lower: "/img".into(),
                upper: "/j/upper".into(),
                work: "/j/work".into(),
            },
            mountpoint: "/j/root".into(),
            binds: vec![("/j/build".into(), "/build".into())],
            network: false,
            cgroup: Some("/sys/fs/cgroup/x".into()),
            hostname: "repoman-build".into(),
            chroot: true,
            netlog: None,
        };
        let cmd = sb
            .command(
                &["makepkg".into(), "--holdver".into()],
                RunAs::User { uid: 1500, gid: 1500 },
                "/build/foo",
                &[("HOME".into(), "/build".into())],
            )
            .unwrap();
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"--net".to_string()));
        assert!(args.contains(&"--kill-child".to_string()));
        let setpriv = args.iter().position(|a| a == "setpriv").unwrap();
        assert_eq!(args[setpriv + 1], "--reuid=1500");
        assert!(args.contains(&"HOME=/build".to_string()));
        assert_eq!(&args[args.len() - 2..], ["makepkg", "--holdver"]);
        let envs: Vec<(String, String)> = std_cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_string_lossy().to_string(), v?.to_string_lossy().to_string())))
            .collect();
        assert!(envs.contains(&("REPOMAN_UPPER".into(), "/j/upper".into())));
        assert!(envs.contains(&("REPOMAN_CWD".into(), "/build/foo".into())));
        assert!(envs.contains(&("REPOMAN_NET".into(), "0".into())));
        assert!(envs.contains(&("REPOMAN_BIND_DST_0".into(), "/build".into())));
        assert!(envs.contains(&("REPOMAN_CGROUP".into(), "/sys/fs/cgroup/x".into())));
    }

    #[test]
    fn dir_root_online_root_user() {
        let sb = Sandbox {
            root: RootSource::Dir("/img".into()),
            mountpoint: "/j/root".into(),
            binds: vec![],
            network: true,
            cgroup: None,
            hostname: "h".into(),
            chroot: true,
            netlog: None,
        };
        let cmd = sb.command(&["true".into()], RunAs::Root, "/", &[]).unwrap();
        let args: Vec<String> = cmd.as_std().get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(!args.contains(&"--net".to_string()));
        assert!(!args.contains(&"setpriv".to_string()));
        assert!(args.contains(&"env".to_string()));
    }

    #[test]
    fn setup_script_is_posix_sh() {
        // Smoke: the script must at least parse under sh -n.
        let out = std::process::Command::new("sh")
            .arg("-n")
            .arg("-c")
            .arg(SETUP_SCRIPT)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
}
