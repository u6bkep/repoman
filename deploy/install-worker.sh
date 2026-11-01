#!/usr/bin/env bash
# Configure and enable the repoman worker service on this host.
#
#   sudo deploy/install-worker.sh [path/to/worker.toml]
#
# Expects the repoman-git package from the private repo to be installed
# (see README.md: add the [repoman] stanza, import the key, pacman -S
# repoman-git). Falls back to installing the locally built binary when the
# package is absent. The config defaults to the invoking user's
# ~/.config/repoman/worker.toml, with scratch_dir rewritten to
# /var/lib/repoman-worker and the user-namespace only pacstrap_gpgdir
# dropped. Stops the user-level unit of the same name first, since both
# would connect as the same worker.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "run with sudo" >&2
    exit 1
fi
repo_dir=$(cd "$(dirname "$0")/.." && pwd)
owner=${SUDO_USER:-}
owner_home=$(getent passwd "${owner:-root}" | cut -d: -f6)
src_config=${1:-$owner_home/.config/repoman/worker.toml}
[[ -f $src_config ]] || { echo "no config at $src_config" >&2; exit 1; }

if pacman -Q repoman-git >/dev/null 2>&1; then
    echo "using packaged repoman $(pacman -Q repoman-git | cut -d' ' -f2)"
    # Leftovers from an earlier fallback install would shadow the package.
    rm -f /etc/systemd/system/repoman-worker.service /usr/local/bin/repoman
else
    bin=$repo_dir/target/release/repoman
    [[ -x $bin ]] || { echo "neither repoman-git nor a release build is available" >&2; exit 1; }
    echo "repoman-git not installed; installing the local build to /usr/local/bin"
    install -m 755 "$bin" /usr/local/bin/repoman
    install -m 644 "$repo_dir/deploy/repoman-worker.service" /etc/systemd/system/repoman-worker.service
    sed -i 's#^ExecStart=/usr/bin/repoman#ExecStart=/usr/local/bin/repoman#' /etc/systemd/system/repoman-worker.service
fi

if [[ -n $owner ]]; then
    systemctl --user -M "$owner@.host" disable --now repoman-worker.service 2>/dev/null || true
fi

install -d -m 700 /etc/repoman
install -d -m 755 /var/lib/repoman-worker
sed -e 's#^scratch_dir = .*#scratch_dir = "/var/lib/repoman-worker"#' \
    -e '/^pacstrap_gpgdir/d' \
    -e '/^# Host-side keyring/d' \
    "$src_config" > /etc/repoman/worker.toml
chmod 600 /etc/repoman/worker.toml
systemctl daemon-reload
# enable --now leaves an already running unit on its old binary; restart
# so the installed one actually serves.
systemctl enable repoman-worker.service
systemctl restart repoman-worker.service
sleep 2
systemctl --no-pager --lines=5 status repoman-worker.service || true
