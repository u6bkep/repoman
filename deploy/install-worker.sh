#!/usr/bin/env bash
# Install the repoman worker as a root systemd service on this host.
#
#   sudo deploy/install-worker.sh [path/to/worker.toml]
#
# The config defaults to the invoking user's ~/.config/repoman/worker.toml,
# with scratch_dir rewritten to /var/lib/repoman-worker and the user-namespace
# only pacstrap_gpgdir dropped. Stops and disables the user-level unit of the
# same name first, since both would connect as the same worker.
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
bin=$repo_dir/target/release/repoman
[[ -x $bin ]] || { echo "build first: cargo build --release" >&2; exit 1; }

if [[ -n $owner ]]; then
    # The user unit must not keep connecting under the same worker name.
    systemctl --user -M "$owner@.host" disable --now repoman-worker.service 2>/dev/null || true
fi

install -m 755 "$bin" /usr/local/bin/repoman
install -d -m 700 /etc/repoman
install -d -m 755 /var/lib/repoman-worker
sed -e 's#^scratch_dir = .*#scratch_dir = "/var/lib/repoman-worker"#' \
    -e '/^pacstrap_gpgdir/d' \
    -e '/^# Host-side keyring/d' \
    "$src_config" > /etc/repoman/worker.toml
chmod 600 /etc/repoman/worker.toml
install -m 644 "$repo_dir/deploy/repoman-worker.service" /etc/systemd/system/repoman-worker.service
systemctl daemon-reload
systemctl enable --now repoman-worker.service
sleep 2
systemctl --no-pager --lines=5 status repoman-worker.service || true
echo
echo "Installed. Update later with:"
echo "  cargo build --release && sudo install -m 755 target/release/repoman /usr/local/bin/repoman && sudo systemctl restart repoman-worker"
