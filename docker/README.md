# Docker / Podman deployment (work in progress)

**Status:** experimental. Running repoman inside a container works, but the
setup hasn't been polished and is not the recommended way to run repoman
right now. Build directly with `cargo build --release` and run on the host
instead — see the top-level `README.md`.

The files in this directory are kept so the containerized path can be picked
back up later. Known rough edges:

- Rootless podman-in-podman requires `--userns=keep-id`, `--security-opt
  label=disable`, `seccomp=unconfined`, and `/dev/fuse` — all set here, but
  every change to host kernel / subuid configuration tends to break it.
- Host data paths are hardcoded to `./data/` relative to the repo. Override
  with `DATA_DIR=/your/path ./docker/start-container.sh` or by editing
  `compose.yaml`.
- The embedded config in `Containerfile` is duplicated from the host config.
  These can drift.

## Files

- `Containerfile` — multi-stage build (rust builder + archlinux runtime)
- `compose.yaml` — compose service definition; build context is the repo root
- `start-container.sh` — convenience script that builds and runs via `podman run`
- `.containerignore` — build-context excludes

## Usage

From the repo root:

```sh
./docker/start-container.sh                # build and run
DATA_DIR=/var/lib/repoman ./docker/start-container.sh   # custom data dir

# or with compose
podman-compose -f docker/compose.yaml up --build
```
