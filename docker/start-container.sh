#!/bin/sh
# NOTE: Containerized deployment is a work in progress and is not the
# recommended way to run repoman right now. See docker/README.md.
#
# DATA_DIR controls where persistent state lives on the host. Override to
# point at your own storage, e.g.:
#   DATA_DIR=/var/lib/repoman ./docker/start-container.sh

set -e

# Run from repo root regardless of where this script is invoked from
cd "$(dirname "$0")/.."

IMAGE="repoman"
DATA_DIR="${DATA_DIR:-$(pwd)/data}"

mkdir -p \
    "$DATA_DIR/repoman" \
    "$DATA_DIR/repo" \
    "$DATA_DIR/cache/aur-clones" \
    "$DATA_DIR/cache/pacman" \
    "$DATA_DIR/cache/sources" \
    "$DATA_DIR/logs" \
    "$DATA_DIR/podman-storage"

echo "Building container image..."
podman build -t "$IMAGE" -f docker/Containerfile .

echo "Starting repoman..."
exec podman run --rm \
    --name repoman \
    --userns=keep-id \
    --security-opt label=disable \
    --security-opt seccomp=unconfined \
    --device /dev/fuse \
    -p 8088:8080 \
    -v "$DATA_DIR/repoman":/data \
    -v "$DATA_DIR/repo":/srv/repo/aur \
    -v "$DATA_DIR/cache/aur-clones":/var/cache/repoman/aur \
    -v "$DATA_DIR/cache/pacman":/var/cache/repoman/pacman \
    -v "$DATA_DIR/cache/sources":/var/cache/repoman/sources \
    -v "$DATA_DIR/logs":/var/log/repoman \
    -v "$DATA_DIR/podman-storage":/home/repoman/.local/share/containers \
    "$@" \
    "$IMAGE"
