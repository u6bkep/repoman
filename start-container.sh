#!/bin/sh
set -e

cd "$(dirname "$0")"

IMAGE="repoman"

echo "Building container image..."
podman build -t "$IMAGE" .

echo "Starting repoman..."
exec podman run --rm \
    --name repoman \
    --userns=keep-id \
    --security-opt label=disable \
    --security-opt seccomp=unconfined \
    --device /dev/fuse \
    -p 8088:8080 \
    -v /media/b-srv0/data/aur/data:/data \
    -v /media/b-srv0/data/aur:/srv/repo/aur \
    -v /media/b-srv0/data/aur/cache/aur-clones:/var/cache/repoman/aur \
    -v /media/b-srv0/data/aur/cache/pacman:/var/cache/repoman/pacman \
    -v /media/b-srv0/data/aur/cache/sources:/var/cache/repoman/sources \
    -v /media/b-srv0/data/aur/logs:/var/log/repoman \
    -v /media/b-srv0/data/aur/podman-storage:/home/repoman/.local/share/containers \
    "$@" \
    "$IMAGE"
