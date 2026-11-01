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
runs as root under systemd. repoman ships as the `repoman-git` package built
by repoman itself from this branch (root `PKGBUILD`), so a build host installs
it like any other package once the client stanza below is in place. The
coordinator learns about it as a git-sourced package; the fragment matters
because GitHub's default branch is still v1:

```sh
repoman add repoman-git --source 'https://github.com/u6bkep/repoman.git#branch=v2-main'
```

Then on each build host:

```sh
sudo pacman -Syu repoman-git
sudo deploy/install-worker.sh          # config from ~/.config/repoman/worker.toml
```

The package installs `/usr/bin/repoman`, the hardened unit, and an example
`/etc/repoman/worker.toml`. The script writes the real config (token,
coordinator URL), enables the unit, and stops the user-level unit so only one
process connects under the worker's name. Without the package it falls back to
installing a local release build under `/usr/local/bin`.

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

## Package cache for clients

The coordinator serves upstream packages through `/cache` as a pacman
`CacheServer`: the first machine to need a file pulls it across the internet,
every later one gets it from the NAS. Build roots use it automatically. To
let a client use it too, add one line to each distribution stanza in
`/etc/pacman.conf`, above its `Include`:

```
[core]
CacheServer = http://repoman.b.intern.gecko.network:7800/cache
Include = /etc/pacman.d/mirrorlist
```

pacman tries the cache first and falls back to the mirrors on any failure,
so a coordinator outage costs nothing but speed.

## Diagnosing network attempts in offline builds

build(), check(), and package() run with no route to the internet: a sink
interface takes the default route and nftables counts and rejects whatever
reaches it. When the count is non-zero the job log says so and lists the
packets (`net: ...` lines): with `tcpdump` on the worker the lines include
DNS question names, which usually point straight at the build step that
tried to fetch; without it the nftables trace shows addresses and ports. The
raw capture (`netlog.json.pcap` or `.trace`) stays in the job's scratch
directory, kept for failed jobs. Any attempt during an offline build holds
the candidate in publish_review; a package that legitimately needs the
network at build time gets `allow_network_build` in its policy instead.
