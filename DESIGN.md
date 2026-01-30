# Repoman Design Document

A modern AUR package repository manager with containerized builds and web interface.

## Overview

Repoman manages a local Arch Linux repository populated with packages built from the AUR. It provides:

- Git-based AUR package tracking
- PKGBUILD review workflow with diff support
- Isolated builds in Podman containers
- Web interface for management
- REST API for automation

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Web Frontend                         │
│  (Static HTML + Vanilla JS, served by Axum)            │
└─────────────────────┬───────────────────────────────────┘
                      │ REST API + WebSocket (logs)
┌─────────────────────▼───────────────────────────────────┐
│                   Rust Backend                          │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐       │
│  │ AUR Service │ │Build Service│ │ Repo Service│       │
│  │ - RPC API   │ │ - Podman    │ │ - repo-add  │       │
│  │ - Git ops   │ │ - Logs      │ │ - Files     │       │
│  └─────────────┘ └─────────────┘ └─────────────┘       │
│                         │                               │
│              ┌──────────▼──────────┐                   │
│              │   SQLite Database   │                   │
│              └─────────────────────┘                   │
└─────────────────────────────────────────────────────────┘
        │                   │                    │
┌───────▼───────┐  ┌────────▼────────┐  ┌───────▼───────┐
│  AUR Git      │  │ Podman          │  │ Local Repo    │
│  Clones       │  │ Containers      │  │ (files)       │
│  ~/.cache/    │  │                 │  │ /srv/repo/    │
│  repoman/aur/ │  │ Shared cache:   │  │               │
└───────────────┘  │ - pacman pkgs   │  │ Served via:   │
                   │ - sources       │  │ - nginx       │
                   └─────────────────┘  │ - NFS         │
                                        └───────────────┘
```

## Technology Stack

| Component       | Technology                          |
|-----------------|-------------------------------------|
| Language        | Rust                                |
| Web Framework   | Axum                                |
| Database        | SQLite (via sqlx or rusqlite)       |
| Containers      | Podman (rootless)                   |
| Shell Commands  | xshell or std::process::Command     |
| Config          | TOML (via serde)                    |
| Frontend        | Static HTML + Vanilla JS            |
| Syntax Highlight| syntect (server-side rendering)     |

---

## Features

### Legend

| Symbol | Meaning |
|--------|---------|
| `[v1]` | In scope for version 1 |
| `[v2]` | Future version |
| `[  ]` | Not yet validated |
| `[x]` | Validated/complete |

---

## 1. Package Management

### 1.1 Add Package `[v1]`

Add an AUR package to be tracked by Repoman.

**Behavior:**
- Query AUR RPC API to verify package exists (can be `pkgname` or `pkgbase`)
- Resolve `pkgname` → `pkgbase` (the build unit)
- If `pkgbase` already tracked, return existing record
- Clone git repository from `https://aur.archlinux.org/{pkgbase}.git`
- Parse `.SRCINFO` for metadata:
  - All `pkgname` entries → store in `package_names` table
  - `pkgver`, `pkgrel`, `depends`, `makedepends`, etc.
- If package has AUR dependencies not in repo, prompt to add them recursively
  - Dependencies added this way get `install_reason = 'dependency'`
- Set package status to `PendingReview`
- Set `install_reason` to `'explicit'` (default) or `'dependency'` if `--asdeps` flag

**Validation Criteria:**
- [x] `POST /api/packages` with `{"name": "foo"}` creates package record
- [x] Adding by `pkgname` resolves to and tracks `pkgbase`
- [x] Adding same `pkgbase` twice returns existing record (idempotent)
- [x] Git repo cloned to configured cache directory
- [x] `.SRCINFO` parsed: all `pkgnames` stored in `package_names` table
- [x] Returns error if package doesn't exist in AUR
- [x] Detects and lists missing AUR dependencies (runs in build container)
- [x] Default `install_reason` is `'explicit'`
- [x] `{"name": "foo", "as_dependency": true}` sets `install_reason = 'dependency'`

### 1.2 Remove Package `[v1]`

Remove a package from tracking and optionally from repo.

**Behavior:**
- Resolve `pkgname` → `pkgbase` if needed
- Remove `pkgbase` record from database (cascades to `package_names`)
- Optionally delete git clone
- Optionally run `repo-remove` for **all** `pkgnames` and delete files
- Check for dependent packages and warn
- With `--recursive` flag: also remove orphaned dependencies (packages with `install_reason = 'dependency'` that are no longer needed)

**Validation Criteria:**
- [x] `DELETE /api/packages/{pkgbase}` removes database record
- [x] Also works with `pkgname` (resolves to `pkgbase`)
- [x] Query param `?delete_files=true` removes all split package files from repo
- [x] Query param `?delete_clone=true` removes git clone
- [x] Returns warning if other tracked packages depend on any `pkgname`
- [x] Query param `?recursive=true` also removes orphaned dependencies

### 1.3 List Packages `[v1]`

List all tracked packages with status.

**Validation Criteria:**
- [x] `GET /api/packages` returns list of `pkgbase` entries
- [x] Each entry includes: `pkgbase`, `pkgnames[]`, `status`, `pkgver`, `approved_commit`, `install_reason`
- [x] Supports filtering by status: `?status=UpdateAvailable`
- [x] Supports filtering by install reason: `?reason=explicit` or `?reason=dependency`
- [x] Supports `?orphans=true` to list only orphaned dependencies (via `/api/packages/orphans`)
- [x] Includes dependency information (aggregated from all `pkgnames`)

### 1.4 Package Details `[v1]`

Get detailed info about a specific package.

**Validation Criteria:**
- [x] `GET /api/packages/{pkgbase}` returns full package info
- [x] Also works with `pkgname` (resolves to `pkgbase`)
- [x] Includes all `pkgnames` with their individual metadata
- [x] Includes parsed `.SRCINFO` data (makedepends, sources, etc.)
- [x] Includes build history (last N builds)
- [x] Includes AUR metadata (votes, popularity, maintainer, out-of-date flag)

### 1.5 Search AUR `[v1]`

Search AUR for packages (for adding new ones).

**Validation Criteria:**
- [x] `GET /api/aur/search?q=term` returns AUR search results
- [x] Returns: name, pkgbase, version, description, votes, popularity, maintainer
- [x] Indicates if `pkgbase` is already tracked

---

## 2. Review Workflow

### 2.1 View PKGBUILD `[v1]`

View the current PKGBUILD content.

**Validation Criteria:**
- [x] `GET /api/packages/{name}/pkgbuild` returns PKGBUILD content
- [x] Returns current git commit hash
- [x] Returns file list in repo (PKGBUILD, .SRCINFO, install scripts, patches)

### 2.2 View PKGBUILD Diff `[v1]`

View changes since last approved commit.

**Validation Criteria:**
- [x] `GET /api/packages/{name}/diff` returns unified diff
- [x] Compares approved_commit to current HEAD
- [x] If no approved_commit, returns full file as "new"
- [x] Includes all changed files, not just PKGBUILD

### 2.3 Approve PKGBUILD `[v1]`

Mark current commit as reviewed and approved.

**Behavior:**
- Store current HEAD as `approved_commit`
- Update status to `Approved` (ready to build)
- If `auto_approve` is set, future updates auto-approve

**Validation Criteria:**
- [x] `POST /api/packages/{name}/approve` stores commit hash
- [x] Status changes to `Approved`
- [x] Optional `?commit=abc123` to approve specific commit
- [x] Records who approved and when (for audit)

### 2.4 Reject PKGBUILD `[v1]`

Mark package as rejected (won't build).

**Validation Criteria:**
- [x] `POST /api/packages/{name}/reject` with `{"reason": "..."}`
- [x] Status changes to `Rejected`
- [x] Reason stored for reference

### 2.5 Auto-Approve Setting `[v1]`

Configure package to auto-approve updates.

**Validation Criteria:**
- [x] `PATCH /api/packages/{name}` with `{"auto_approve": true}`
- [x] When updates detected, automatically approves if flag set
- [x] Still records the commit that was auto-approved

---

## 3. Build System

### 3.1 Trigger Build `[v1]`

Start a build for an approved package.

**Behavior:**
1. Verify package is in `Approved` status
2. Resolve dependencies (install from official repos + our repo)
3. Create/start Podman container
4. Run `makepkg -s` inside container
5. Extract `.pkg.tar.zst` files
6. Run `repo-add` to update repository database
7. Update package status and repo_version

**Validation Criteria:**
- [x] `POST /api/packages/{name}/build` starts build
- [x] Returns build ID for tracking
- [x] Build runs in Podman container
- [x] Package files appear in repo directory
- [x] Repo database updated with `repo-add`
- [x] Build logs captured and stored
- [x] Status updates: `Approved` -> `Building` -> `UpToDate` or `Failed`
- [x] Optional tmpfs for build directory (`tmpfs_build = true` in config)

### 3.2 Build Queue `[v1]`

Queue multiple builds in dependency order.

**Validation Criteria:**
- [x] `POST /api/builds/queue` with `{"packages": ["a", "b", "c"]}`
- [x] Topologically sorts by dependencies
- [x] Builds execute sequentially in correct order
- [x] If build fails, dependent builds are skipped

### 3.3 View Build Status `[v1]`

Check status of a running or completed build.

**Validation Criteria:**
- [x] `GET /api/builds/{id}` returns build info
- [x] Includes: status, started_at, finished_at, commit, package outputs
- [x] For running builds, includes current stage

### 3.4 Stream Build Logs `[v1]`

Stream logs from a running build.

**Validation Criteria:**
- [x] `GET /api/builds/{id}/logs` returns complete logs (if finished)
- [x] WebSocket `/api/builds/{id}/logs/stream` streams live logs
- [x] Logs persisted after build completes

### 3.5 Cancel Build `[v1]`

Cancel a running build.

**Validation Criteria:**
- [x] `POST /api/builds/{id}/cancel` stops container
- [x] Status set to `Cancelled`
- [x] Partial artifacts cleaned up (pkgdest cleaned at next build start, container auto-removed via `--rm`)

### 3.6 Build History `[v1]`

View past builds for a package.

**Validation Criteria:**
- [x] `GET /api/packages/{name}/builds` returns build history
- [x] Includes: build ID, commit, status, duration, timestamp
- [x] Paginated with `?limit=N&offset=M`

---

## 4. Container Management

### 4.1 Base Image `[v1]`

Maintain a base container image for builds.

**Behavior:**
- Based on `archlinux:base-devel`
- Adds: `git`, package signing keys, locale setup
- Configures pacman with our local repo
- Creates unprivileged build user

**Validation Criteria:**
- [x] `POST /api/system/image/rebuild` creates new base image
- [x] Image includes working `makepkg` setup
- [x] Image configured to use local repo for dependencies
- [x] GPG keys for common packagers pre-imported
- [x] Image tagged with build date for versioning

### 4.2 Pacman Cache `[v1]`

Shared pacman download cache between builds.

**Behavior:**
- Mount shared volume at `/var/cache/pacman/pkg` in containers
- Cache persists across builds
- Cleanup keeps only the last 2 versions of each package (paccache-style)

**Validation Criteria:**
- [x] Downloaded packages reused across builds
- [x] `POST /api/system/cache/clean` removes old package versions (keeps last 2)
- [x] `GET /api/system/status` includes pacman cache size and package count
- [x] Frontend displays cache stats and cleanup button

### 4.3 Source Cache `[v1]`

Cache downloaded source tarballs (SRCDEST).

**Behavior:**
- Each package has a persistent source directory at `~/.cache/repoman/sources/{pkgbase}/`
- Mounted into build container at `/build/sources` (makepkg's SRCDEST)
- Sources persist across rebuilds of the same version
- On version bump: selectively delete sources whose checksums changed
- On package removal with `--delete-clone`: clean all sources for that package

**Validation Criteria:**
- [x] Sources cached in per-package directories
- [x] Reused when rebuilding same version
- [x] Selective cleanup on version bump (compares SRCINFO checksums)
- [x] Full cleanup on package removal with `--delete-clone`

### 4.4 Orphan Cleanup `[v1]`

Clean up orphaned data from removed packages.

**Behavior:**
- Detects directories/files that don't correspond to any tracked package
- Orphan types: AUR clones, source caches, pkgdest directories, build logs
- System status shows orphan count and total size
- Single endpoint cleans all orphan types

**Validation Criteria:**
- [x] `GET /api/system/status` includes `orphaned_data` with lists and total size
- [x] `POST /api/system/orphans/clean` removes all orphaned data
- [x] Handles subuid permissions via `podman unshare` where needed
- [x] Frontend shows warning when orphans exist

### 4.5 Container Cleanup `[v1]`

Clean up stopped containers and images.

**Behavior:**
- Lists only repoman-related containers (filtered by name prefix `repoman-build-`)
- Lists only repoman builder images (filtered by repository name)
- Image pruning keeps last N images (default 3) by date tag
- Container pruning removes all stopped build containers

**Validation Criteria:**
- [x] `GET /api/system/containers` lists repoman build containers
- [x] `POST /api/system/containers/prune` removes stopped build containers
- [x] `GET /api/system/images` lists repoman builder images
- [x] `POST /api/system/images/prune?keep=N` removes old images (keeps last N)
- [x] Frontend displays containers and images with prune buttons

---

## 5. Update Detection

### 5.1 Check for Updates `[v1]`

Check tracked packages for upstream changes.

**Behavior:**
- `git fetch` for each tracked package
- Compare local HEAD to remote HEAD
- If different, update status to `UpdateAvailable`
- If auto_approve set, automatically approve

**Validation Criteria:**
- [x] `POST /api/packages/check-updates` fetches all repos
- [x] Returns list of packages with updates
- [x] Updates package status in database
- [x] Auto-approves packages with auto_approve flag

### 5.2 Scheduled Update Checks `[v2]`

Automatically check for updates on schedule.

**Validation Criteria:**
- [ ] Config option for check interval (e.g., `update_check_interval = "6h"`)
- [ ] Background task runs checks automatically
- [ ] Notifications sent when updates found

### 5.3 VCS Package Rebuilds `[v2]`

Handle `-git`, `-svn`, `-hg` packages that need periodic rebuilds.

**Validation Criteria:**
- [ ] Detect VCS packages by pkgname suffix
- [ ] Config option for rebuild interval
- [ ] Track last rebuild time
- [ ] Trigger rebuild even if PKGBUILD unchanged

### 5.4 Dependency Update Detection `[v2]`

Detect when official repo updates break AUR packages.

**Validation Criteria:**
- [ ] Track key dependency versions (python, ruby, etc.)
- [ ] Flag packages needing rebuild after major version bumps
- [ ] Optional auto-rebuild on dependency updates

---

## 6. Repository Management

### 6.1 Repository Info `[v1]`

Get information about the managed repository.

**Validation Criteria:**
- [x] `GET /api/repo` returns: path, package count, total size
- [x] Lists all packages with versions (returned in `packages` array with pkgname, version, arch)

### 6.2 Remove from Repo `[v1]`

Remove a package from the repository (but keep tracking).

**Validation Criteria:**
- [x] `DELETE /api/repo/packages/{name}` removes from repo
- [x] Runs `repo-remove` (via package delete with ?delete_files=true)
- [x] Deletes `.pkg.tar.zst` files
- [x] Package record kept with status `Approved` (can rebuild)

### 6.3 Clean Old Versions `[v1]`

Remove old package versions from repo.

**Validation Criteria:**
- [x] `POST /api/repo/clean` removes old versions
- [x] Config option `keep_versions = 2` controls retention
- [x] Only latest N versions kept

### 6.4 Rebuild Database `[v1]`

Regenerate repo database from files.

**Validation Criteria:**
- [x] `POST /api/repo/rebuild-db` regenerates `.db` and `.files`
- [x] Scans repo directory for `.pkg.tar.zst` files
- [x] Useful for recovery or manual file changes

---

## 7. Web Interface

### 7.1 Dashboard `[v1]`

Overview of repository status.

**Validation Criteria:**
- [x] Shows package counts by status
- [x] Lists packages needing attention (pending review, updates available, failed)
- [x] Shows current/recent builds

### 7.2 Package List `[v1]`

Browseable list of tracked packages.

**Validation Criteria:**
- [x] Sortable by name, status, last updated
- [x] Filterable by status
- [x] Quick actions: build, check update, remove

### 7.3 Package Detail View `[v1]`

Detailed view for a single package.

**Validation Criteria:**
- [x] Shows current PKGBUILD with syntax highlighting
- [x] Shows diff view when updates available
- [x] Approve/reject buttons
- [x] Build history with log links
- [x] AUR info (votes, maintainer, etc.)

### 7.4 Add Package Form `[v1]`

Form to add new packages.

**Validation Criteria:**
- [x] Search box queries AUR
- [x] Shows search results with key info
- [x] Click to add, shows PKGBUILD for review
- [x] Option to add dependencies recursively

### 7.5 Build Log Viewer `[v1]`

View build logs.

**Validation Criteria:**
- [x] Syntax highlighting for build output (client-side pattern matching)
- [x] Auto-scroll for live logs
- [x] Search within logs
- [x] Download raw log

### 7.6 Settings Page `[v1]`

Configure Repoman.

**Validation Criteria:**
- [x] View/edit configuration (view current config & template, no direct editing)
- [x] Rebuild base image button
- [x] Clean cache button
- [x] System status (disk usage, container status)

### 7.7 Frontend Implementation `[v1]`

Static HTML + Vanilla JS served by Axum.

**Structure:**
```
static/
├── index.html              # Dashboard / SPA shell
├── css/
│   └── style.css           # All styles
└── js/
    ├── api.js              # API client (fetch wrapper)
    ├── router.js           # Simple hash-based routing
    ├── components/
    │   ├── package-list.js
    │   ├── package-detail.js
    │   ├── build-log.js
    │   └── ...
    └── main.js             # Entry point, initializes app
```

**Patterns:**
- Single HTML file with `<template>` elements for reusable markup
- Hash-based routing (`#/packages`, `#/packages/foo`, `#/builds/123`)
- ES modules (`import`/`export`) - no bundler needed
- API client wraps `fetch()` with error handling and base URL
- WebSocket wrapper for log streaming with auto-reconnect
- Syntax highlighting rendered server-side by `syntect` (returned as HTML)
- Diff rendered server-side (returned as HTML with line-by-line markup)

**API Client Example:**
```javascript
// js/api.js
const API_BASE = '/api';

export async function get(path) {
    const res = await fetch(`${API_BASE}${path}`);
    if (!res.ok) throw new Error(`${res.status}: ${await res.text()}`);
    return res.json();
}

export async function post(path, body) {
    const res = await fetch(`${API_BASE}${path}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`${res.status}: ${await res.text()}`);
    return res.json();
}

export const packages = {
    list: () => get('/packages'),
    get: (name) => get(`/packages/${name}`),
    add: (name) => post('/packages', { name }),
    approve: (name) => post(`/packages/${name}/approve`),
    build: (name) => post(`/packages/${name}/build`),
};
```

**WebSocket Log Streaming:**
```javascript
// js/components/build-log.js
export function streamLogs(buildId, container) {
    const ws = new WebSocket(`ws://${location.host}/api/builds/${buildId}/logs/stream`);
    ws.onmessage = (e) => {
        container.textContent += e.data;
        container.scrollTop = container.scrollHeight;
    };
    ws.onclose = () => container.textContent += '\n[Connection closed]';
    return () => ws.close();
}
```

**Validation Criteria:**
- [x] All pages load without JavaScript errors
- [x] Navigation works via hash routes
- [x] API errors displayed to user
- [x] Log streaming connects and displays live output
- [ ] Works in Firefox and Chromium (not tested)

---

## 8. Notifications `[v2]`

### 8.1 Webhook Notifications

**Validation Criteria:**
- [ ] Config option for webhook URL
- [ ] Sends POST on: build complete, build failed, updates available
- [ ] Configurable events

### 8.2 ntfy.sh Integration

**Validation Criteria:**
- [ ] Config option for ntfy topic
- [ ] Sends notification on configurable events

---

## 9. Security `[v2]`

### 9.1 Network Isolation

Disable network during package build phase.

**Validation Criteria:**
- [ ] Sources downloaded before build
- [ ] `makepkg` runs with `--network=none`
- [ ] Optional per-package override for packages needing network

### 9.2 Resource Limits

Limit container resources.

**Validation Criteria:**
- [ ] Config options for memory limit, CPU limit
- [ ] Build timeout (kill after N minutes)
- [ ] Limits applied via Podman flags

### 9.3 PKGBUILD Security Scanning `[v2]`

Highlight potentially dangerous patterns.

**Validation Criteria:**
- [ ] Warns on: `curl | sh`, `rm -rf /`, unusual permissions
- [ ] Displayed in review UI
- [ ] Optional block on dangerous patterns

---

## 10. CLI Interface `[v1]`

Command-line client for API.

### 10.1 Package Commands

```bash
repoman add <package>          # Add package (explicit)
repoman add --asdeps <package> # Add package as dependency
repoman remove <package>       # Remove package
repoman remove -s <package>    # Remove package and orphaned deps (like pacman -Rs)
repoman list                   # List packages
repoman list --explicit        # List explicitly installed only
repoman list --deps            # List dependencies only
repoman list --orphans         # List orphaned dependencies
repoman info <package>         # Package details
repoman search <query>         # Search AUR
repoman mark --explicit <pkg>  # Mark as explicitly installed
repoman mark --asdeps <pkg>    # Mark as dependency
```

**Validation Criteria:**
- [x] All commands work and produce readable output
- [x] `--json` flag for machine-readable output
- [x] Proper exit codes
- [x] `--asdeps` flag sets install_reason correctly
- [x] `--orphans` correctly identifies unneeded dependencies

### 10.2 Review Commands

```bash
repoman review <package>       # Show PKGBUILD for review
repoman diff <package>         # Show diff
repoman approve <package>      # Approve current commit
repoman reject <package>       # Reject with reason
```

**Validation Criteria:**
- [x] `review` opens in $EDITOR or prints to stdout (`--stdout` flag)
- [x] `diff` shows colored unified diff
- [x] All commands update package status

### 10.3 Build Commands

```bash
repoman build <package>        # Build single package
repoman build <pkg1> <pkg2>    # Build multiple packages (queue with dep ordering)
repoman build --all            # Build all approved
repoman build --updates        # Build packages with updates
repoman logs <build-id>        # View build logs
repoman cancel <build-id>      # Cancel running build
```

**Validation Criteria:**
- [x] `build` shows progress and final status
- [x] Multiple packages are topologically sorted and built in order
- [x] Failed builds skip their dependents
- [x] `--follow` streams logs (via `repoman logs --follow`)
- [x] Exit code reflects build success/failure

### 10.4 Maintenance Commands

```bash
repoman check-updates          # Check all for updates
repoman clean-cache            # Clean pacman cache
repoman rebuild-image          # Rebuild base container image
repoman status                 # System status overview
```

**Validation Criteria:**
- [x] All commands work and provide feedback

---

## 11. Configuration

### 11.1 Config File

Location: `~/.config/repoman/config.toml` or `/etc/repoman/config.toml`

```toml
[repo]
# Path to repository directory
path = "/srv/repo/aur"
# Repository name (used in pacman.conf)
name = "aur-local"
# Number of old package versions to keep
keep_versions = 2

[cache]
# Path to AUR git clones
aur_clones = "~/.cache/repoman/aur"
# Path to shared pacman package cache
pacman_cache = "~/.cache/repoman/pacman"
# Path to build logs
logs = "~/.local/share/repoman/logs"
# Path to downloaded sources (SRCDEST) - per-package subdirectories
sources = "~/.cache/repoman/sources"

[container]
# Base image name
image = "repoman-builder"
# Use tmpfs (ramdisk) for build directory - faster compilation but uses RAM
tmpfs_build = false
# Size limit for tmpfs (e.g., "4G", "8G") - omit for no limit
# tmpfs_size = "8G"
# Podman socket (optional, for remote)
# socket = "unix:///run/user/1000/podman/podman.sock"

[server]
# API server bind address
listen = "127.0.0.1:8080"

[updates]
# Auto-check interval (0 to disable)
# check_interval = "6h"
```

**Validation Criteria:**
- [x] Config file parsed on startup
- [x] Sensible defaults for all options
- [x] Environment variables can override (`REPOMAN_REPO_PATH`, etc.)
- [x] Config validation with helpful error messages

---

## 12. Split Package Handling

A single PKGBUILD can produce multiple packages (e.g., `qt6-base` and `qt6-base-debug`).

**Terminology:**
- `pkgbase` - The build unit (PKGBUILD identity), used for git clone directory name
- `pkgname` - Individual package names produced by the build

**Design Principles:**
1. **Track by pkgbase** - The `packages` table uses `pkgbase` as primary key
2. **Resolve by pkgname** - Users can add packages by any `pkgname`; we resolve to `pkgbase`
3. **Atomic builds** - Always build entire `pkgbase`, producing all `pkgnames`
4. **Dependency resolution** - Resolve at `pkgname` level (deps vary per split), queue by `pkgbase`
5. **Single repo-add** - Pass all outputs from one build in single call

**Example:**
```
User runs: repoman add qt6-base-debug
  → Query AUR: qt6-base-debug has pkgbase=qt6-base
  → Clone: https://aur.archlinux.org/qt6-base.git
  → Track: packages.pkgbase = "qt6-base"
  → Parse .SRCINFO: produces qt6-base, qt6-base-debug
  → Store in package_names table

Build produces:
  → qt6-base-6.7.0-1-x86_64.pkg.tar.zst
  → qt6-base-debug-6.7.0-1-x86_64.pkg.tar.zst
  → Both added to repo in single repo-add call
```

---

## 13. Install Reason Tracking `[v1]`

Like pacman, Repoman tracks whether packages were explicitly installed or pulled in as dependencies.

**Install Reasons:**
- `explicit` - Package was directly added by the user
- `dependency` - Package was added because another AUR package depends on it

**Behavior:**

1. **Adding packages explicitly:**
   - `repoman add foo` marks `foo` as `explicit`
   - User intentionally wants this package

2. **Adding dependencies automatically:**
   - When adding/building a package, if it has AUR dependencies not yet tracked, they are added with `install_reason = 'dependency'`
   - Dependencies of dependencies are also marked as `dependency`

3. **Orphan detection:**
   - A package is an "orphan" if:
     - Its `install_reason` is `dependency`, AND
     - No `explicit` package (directly or transitively) depends on it
   - `repoman list --orphans` shows these packages

4. **Changing install reason:**
   - `repoman mark --explicit foo` - Mark as explicitly installed
   - `repoman mark --dependency foo` - Mark as dependency
   - Similar to `pacman -D --asexplicit` / `--asdeps`

5. **Removal behavior:**
   - `repoman remove foo` - Remove only `foo`
   - `repoman remove --recursive foo` - Remove `foo` and its orphaned dependencies
   - Before removing, warn if other explicit packages depend on it

**Example:**
```
User runs: repoman add paru
  → paru depends on AUR package "libalpm.so" (not in official repos)
  → Repoman detects this and offers to add the dependency
  → User confirms
  → paru added with install_reason = 'explicit'
  → libalpm-git added with install_reason = 'dependency'

Later: repoman remove paru
  → paru removed
  → libalpm-git is now an orphan (nothing depends on it)
  → "repoman list --orphans" shows libalpm-git
  → "repoman remove --orphans" would remove it
```

**Validation Criteria:**
- [x] `repoman add foo` sets `install_reason = 'explicit'`
- [x] AUR dependencies added automatically get `install_reason = 'dependency'` (with `--with-deps` flag)
- [x] `repoman list --orphans` shows dependency packages with no dependents
- [x] `repoman mark --explicit/--dependency` changes install reason
- [x] `repoman remove --recursive` removes package and orphaned deps
- [x] API endpoints support install_reason field

---

## 14. Database Schema

SQLite database at `~/.local/share/repoman/repoman.db`

```sql
-- Build units tracked by Repoman (keyed by pkgbase)
CREATE TABLE packages (
    pkgbase TEXT PRIMARY KEY,
    aur_url TEXT NOT NULL,
    local_path TEXT NOT NULL,
    approved_commit TEXT,
    current_commit TEXT,
    pkgver TEXT,              -- Version from .SRCINFO
    status TEXT NOT NULL DEFAULT 'PendingReview',
    install_reason TEXT NOT NULL DEFAULT 'explicit',  -- 'explicit' or 'dependency'
    auto_approve BOOLEAN NOT NULL DEFAULT FALSE,
    added_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Individual package names produced by each pkgbase
CREATE TABLE package_names (
    pkgname TEXT PRIMARY KEY,
    pkgbase TEXT NOT NULL REFERENCES packages(pkgbase) ON DELETE CASCADE,
    pkgdesc TEXT,
    pkgver TEXT,              -- Can differ from pkgbase pkgver in rare cases
    depends TEXT,             -- JSON array
    optdepends TEXT,          -- JSON array
    provides TEXT,            -- JSON array
    conflicts TEXT,           -- JSON array
    repo_version TEXT         -- Version currently in our repo (NULL if not built)
);

-- Build history
CREATE TABLE builds (
    id TEXT PRIMARY KEY,
    pkgbase TEXT NOT NULL REFERENCES packages(pkgbase),
    commit TEXT NOT NULL,
    status TEXT NOT NULL,     -- Pending, Running, Success, Failed, Cancelled
    started_at TEXT NOT NULL,
    finished_at TEXT,
    log_path TEXT,
    outputs TEXT,             -- JSON array of built package filenames
    error_message TEXT
);

-- Audit log of approvals
CREATE TABLE approvals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pkgbase TEXT NOT NULL REFERENCES packages(pkgbase),
    commit TEXT NOT NULL,
    approved_at TEXT NOT NULL,
    approved_by TEXT,         -- For future multi-user support
    auto_approved BOOLEAN NOT NULL DEFAULT FALSE
);

-- Index for looking up pkgbase by pkgname (common operation)
CREATE INDEX idx_package_names_pkgbase ON package_names(pkgbase);
```

**Validation Criteria:**
- [x] Schema created on first run
- [x] Migrations for schema changes
- [x] Foreign keys enforced
- [x] Adding package by `pkgname` resolves to correct `pkgbase`
- [x] All `pkgnames` from `.SRCINFO` stored in `package_names` table

---

## Implementation Phases

### Phase 1: Core Backend
- [x] Project setup (Cargo, dependencies)
- [x] Configuration loading
- [x] Database setup and migrations
- [x] AUR RPC client
- [x] Git operations (clone, fetch, diff)
- [x] `.SRCINFO` parser

### Phase 2: Build System
- [x] Podman container management
- [x] Base image creation
- [x] Build execution (rootless podman with `:U` mounts)
- [x] Log capture
- [x] `repo-add` integration

### Phase 3: API Server
- [x] Axum server setup
- [x] Package CRUD endpoints
- [x] System status endpoint
- [x] Build endpoints (trigger from API - spawns async build task)
- [x] WebSocket for log streaming

### Phase 4: CLI
- [x] Command structure (add, remove, list, info, diff, approve, build, check-updates, rebuild-image, status)
- [x] Direct database/build access (not via API client)
- [x] Output formatting

### Phase 5: Web UI
- [x] Basic HTML/JS interface (static files served by Axum)
- [x] Package list and detail views
- [x] Build log viewer
- [x] PKGBUILD diff viewer
- [x] Dashboard with status overview
- [x] Add package page with AUR search
- [x] Settings page

### Phase 6: Polish
- [x] Error handling (improved context, logging, no panics in config)
- [ ] Documentation
- [ ] Testing
- [x] Packaging (AUR PKGBUILD with systemd service)

---

## Open Questions

1. ~~**Frontend framework**: Minimal JS, htmx, or full SPA (Leptos/Yew)?~~ **Resolved:** Plain HTML + Vanilla JS
2. ~~**Authentication**: None for v1 (localhost only), or basic auth?~~ **Resolved:** Skip for v1 (localhost only)
3. ~~**Split packages**: How to handle PKGBUILDs producing multiple packages?~~ **Resolved:** Track by `pkgbase`, store `pkgnames` in separate table. See Section 12.
4. ~~**GPG signing**: Should Repoman sign packages, or leave unsigned?~~ **Resolved:** Skip for v1

All initial questions resolved. New questions can be added as they arise.

---

## References

- [aurutils](./references/aurutils) - Existing shell-based AUR helper
- [AUR RPC Interface](https://wiki.archlinux.org/title/Aurweb_RPC_interface)
- [PKGBUILD - ArchWiki](https://wiki.archlinux.org/title/PKGBUILD) - Split packages documentation
- [Arch Package Guidelines](https://wiki.archlinux.org/title/Arch_package_guidelines)
- [Podman](https://podman.io/)
- [repo-add(8)](https://man.archlinux.org/man/repo-add.8)
