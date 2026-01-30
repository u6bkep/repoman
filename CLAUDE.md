# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Repoman is an AUR (Arch User Repository) package repository manager. It tracks AUR packages, provides a PKGBUILD review workflow, runs isolated builds in rootless Podman containers, and serves a web interface for management.

## Build and Run Commands

```bash
# Build
cargo build
cargo build --release

# Run CLI commands
./target/release/repoman <command>
./target/release/repoman add <package>        # Add AUR package
./target/release/repoman list                  # List tracked packages
./target/release/repoman build <package>       # Build package in container
./target/release/repoman serve                 # Start web server (port 8088)

# Check compilation without building
cargo check
```

## Architecture

```
src/
├── main.rs       # CLI entry point (clap), command handlers
├── config.rs     # TOML config loading (~/.config/repoman/config.toml)
├── models/       # Data structures: Package, Build, SrcInfo, PackageStatus
├── db/           # SQLite via sqlx (async), runtime migrations
├── aur/          # AUR RPC client, git operations, .SRCINFO parser
├── build/        # Podman container builds, repo-add integration
├── repo/         # Repository file management
└── api/          # Axum REST API handlers

static/           # Web UI (vanilla JS, served by Axum)
├── index.html    # SPA shell with <template> elements
├── css/style.css
└── js/
    ├── api.js    # Fetch wrapper
    ├── router.js # Hash-based routing (#/packages, #/packages/foo)
    └── components/
```

## Key Concepts

- **pkgbase vs pkgname**: A single PKGBUILD (pkgbase) can produce multiple packages (pkgnames). Track by pkgbase, resolve by pkgname.
- **Package status flow**: `PendingReview` → `Approved` → `Building` → `UpToDate` or `Failed`
- **Install reason**: `explicit` (user added) vs `dependency` (auto-added). Orphans are dependencies with no dependents.

## Database

SQLite at `~/.local/share/repoman/repoman.db`. Uses sqlx with compile-time query checking disabled (runtime mode). Migrations run automatically on startup in `db/mod.rs`.

## Gotchas

1. **Rootless Podman permissions**: Use `:U` suffix on volume mounts to fix permission issues (e.g., `-v /path:/container/path:U`). This chowns the directory to match the container user's UID.

2. **SRCDEST path**: Don't use `/build/src` as SRCDEST since that's where the read-only source mount goes. Use `/build/sources` instead.

3. **Deleting files after build**: Files created in containers have subuid ownership. Use `podman unshare rm -rf` to delete them from the host, or tolerate deletion failures.

4. **AUR edition requirement**: Cargo.toml uses `edition = "2024"` which requires a recent Rust nightly or stable 1.85+.

5. **Server port**: Default configured port is 8080, but actual listening port may be 8088 depending on config. Check the startup log.

6. **SRCINFO parsing**: Dependencies in the global section (before `pkgname =`) go to `SrcInfo.depends`. Per-package deps go to `SrcInfoPackage.depends`. Most packages have deps only in the global section.

7. **API response types**: The API returns `install_reason` as a string ("explicit" or "dependency"), not an enum. Parse accordingly in the frontend.

8. **Dependency detection runs in container**: When adding a package, missing AUR dependencies are detected by running `pacman -Sddp` inside the build container. This ensures deps are checked against the same repos (official + our local repo) that will be used during builds.

9. **Tmpfs builds**: Set `tmpfs_build = true` in `[container]` config to use ramdisk for the build directory. Faster compilation but uses RAM. Optionally set `tmpfs_size = "8G"` to limit memory usage.

## Config Paths

- Config: `~/.config/repoman/config.toml`
- Database: `~/.local/share/repoman/repoman.db`
- AUR clones: `~/.cache/repoman/aur/`
- Build logs: `~/.local/share/repoman/logs/`
- Pacman cache: `~/.cache/repoman/pacman/`
- Default repo: `/srv/repo/aur/`
