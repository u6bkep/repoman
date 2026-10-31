# Deploying repoman v2

Three pieces: the coordinator container on the NAS, a worker on each build
host, and pacman configuration on each client.

## Coordinator (NAS, docker)

The stack lives at `/containers/repoman` on b-srv0 following that host's
conventions (see `compose.yaml`). To redeploy after a code change:

```sh
rsync -a --delete --exclude target --exclude .git --exclude references \
    ~/Documents/programmingPlay/repoman/ /media/b-srv0/data/containers/repoman/src/
ssh b-srv0 'cd /containers/repoman && docker compose up -d --build'
```

`coordinator.toml` holds the worker tokens and stream definitions; see
`coordinator.example.toml`. The SQLite database and GPG homes are on the SSD
at `/srv/db/repoman/coordinator`; store, repos, images, and caches are on the
array at `/media/data/repoman`.

### Signing key

Once, after the coordinator is up:

```sh
repoman --url http://repoman.b.intern.gecko.network:7800 keygen
```

Put the printed fingerprint into `coordinator.toml` as `signing_key` and
restart the stack. Until then packages are published unsigned and clients
must use `SigLevel = Optional` for the repoman repos.

## Worker (build host)

The executor needs mounts, namespaces, and cgroup delegation, so the worker
runs as root under systemd. One-time setup:

```sh
cargo build --release
sudo deploy/install-worker.sh          # uses ~/.config/repoman/worker.toml
```

The script installs the binary, derives `/etc/repoman/worker.toml` from the
user config, installs the hardened unit, and stops the user-level unit so
only one process connects under the worker's name.

Iteration afterwards is a release build, `install` of the binary, and
`systemctl restart repoman-worker`.

### Without root: user namespaces

For development the worker also runs unprivileged inside a user namespace,
given a subuid range in `/etc/subuid`:

```sh
pacman-key --gpgdir "$W/gnupg" --init && pacman-key --gpgdir "$W/gnupg" --populate archlinux manjaro
unshare -Ur --map-auto -m -- repoman worker --config "$W/worker.toml"
```

with `pacstrap_gpgdir = "$W/gnupg"` in the worker config, because the host's
root-owned keyring is unreadable inside the namespace. cgroup limits and
peak accounting are unavailable in this mode; everything else works. As a
user systemd unit:

```ini
[Service]
ExecStart=/usr/bin/unshare -Ur --map-auto -m -- %h/.local/bin/repoman worker --config %h/.config/repoman/worker.toml
Restart=always
```

Files under the scratch directory end up owned by subordinate uids; remove
them from inside a namespace (`unshare -Ur --map-auto rm -rf ...`).

## Clients

Each machine tracks one stream. Replace the old `[aur]` stanza with:

```ini
[repoman]
SigLevel = Required DatabaseRequired
Server = http://repoman.b.intern.gecko.network:7800/repos/manjaro-stable/$arch
```

using `manjaro-unstable` on the desktop. Import the signing key once:

```sh
curl -s http://repoman.b.intern.gecko.network:7800/repos/repoman.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key <fingerprint>
```

Until a signing key is configured, use `SigLevel = Optional TrustAll` for
this repo only.
