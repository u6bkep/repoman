# Repoman V2 Spec: Derived State Architecture

## Problem Statement

V1 stores package metadata, version info, dependency graphs, and workflow status in SQLite.
This duplicates data that already exists in authoritative sources (git clones, .SRCINFO files,
repo.db.tar). The duplication causes sync bugs, forces JSON-in-TEXT columns for deps, and makes
the status field an overloaded state machine that's hard to extend.

## Design Principle

**The database tracks decisions, not data.**

The DB records what the user decided (track this package, approve this commit, auto-approve
updates). Everything else is derived at runtime from the actual sources of truth.

## Sources of Truth

| Data                          | V1 Source             | V2 Source                        |
|-------------------------------|-----------------------|----------------------------------|
| Which packages are tracked    | DB `packages`         | DB `packages` (thinner)          |
| User prefs (auto_approve, install_reason) | DB `packages` | DB `packages`                |
| Package metadata (deps, desc) | DB `package_names` (JSON blobs) | .SRCINFO from git clone |
| What's in the repo            | DB `package_names.repo_version` | `repo.db.tar` (parsed)  |
| Current upstream commit       | DB `packages.current_commit` | `git rev-parse HEAD` on clone |
| Package version               | DB `packages.pkgver`  | .SRCINFO `pkgver` field          |
| Package status                | DB `packages.status`  | Derived from commit + build + repo state |
| Build history                 | DB `builds`           | DB `builds` (unchanged)          |
| Approval history              | DB `approvals`        | DB `approvals` (unchanged)       |

---

## Database Schema (V2)

### `packages` table

```sql
CREATE TABLE packages (
    pkgbase         TEXT PRIMARY KEY,
    install_reason  TEXT NOT NULL DEFAULT 'explicit',  -- 'explicit' | 'dependency'
    auto_approve    BOOLEAN NOT NULL DEFAULT FALSE,
    approved_commit TEXT,           -- last reviewed/approved commit hash (NULL = never approved)
    added_at        TEXT NOT NULL,  -- RFC3339
    updated_at      TEXT NOT NULL   -- RFC3339
);
```

Removed columns vs V1:
- `current_commit` -- read from git clone HEAD
- `pkgver` -- read from .SRCINFO
- `status` -- derived at runtime (see Status Derivation)
- `has_upstream` -- detected during update check, not persisted
- `in_official_repos` -- detected during update check, not persisted
- `rejection_reason` -- removed (see Rejection Handling)

### `package_names` table -- REMOVED

Replaced entirely by parsing .SRCINFO from git clones at runtime (with in-memory cache).
The pkgbase-to-pkgname mapping, deps, provides, conflicts, description all come from .SRCINFO.
The repo_version comes from parsing repo.db.tar.

### `builds` table (unchanged)

```sql
CREATE TABLE builds (
    id            TEXT PRIMARY KEY,
    pkgbase       TEXT NOT NULL REFERENCES packages(pkgbase),
    git_commit    TEXT NOT NULL,
    status        TEXT NOT NULL,    -- pending | running | success | failed | cancelled
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    log_path      TEXT,             -- relative filename
    outputs       TEXT,             -- JSON array of built .pkg.tar.* filenames
    error_message TEXT
);
CREATE INDEX idx_builds_pkgbase ON builds(pkgbase);
```

### `approvals` table (unchanged)

```sql
CREATE TABLE approvals (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    pkgbase       TEXT NOT NULL REFERENCES packages(pkgbase),
    git_commit    TEXT NOT NULL,
    approved_at   TEXT NOT NULL,
    approved_by   TEXT,
    auto_approved BOOLEAN NOT NULL DEFAULT FALSE
);
```

---

## Status Derivation

Status is no longer stored. It is computed from comparing real state sources.

### Inputs

1. **Package record** (DB): `approved_commit`, `install_reason`
2. **Clone state** (filesystem): does the clone dir exist? what is HEAD?
3. **Repo state** (repo.db.tar): is this pkgbase in the repo? what version?
4. **Build state** (DB `builds` table): latest build for the approved commit
5. **Active builds** (in-memory): is a build currently running for this pkgbase?

### Derivation Logic

```
derive_status(pkg, clone, repo, builds, active_builds):
    if pkgbase in active_builds:
        return Building

    if clone does not exist:
        return Missing          # need to re-clone

    head = clone.HEAD commit

    if pkg.approved_commit is NULL:
        return PendingReview    # never approved

    if head != pkg.approved_commit:
        return UpdateAvailable  # upstream moved past last approval

    # approved_commit == HEAD, so we're up to date with upstream
    latest_build = most recent build WHERE git_commit = pkg.approved_commit

    if latest_build is NULL or latest_build.status == cancelled:
        return Approved         # approved but never built (or last build was cancelled)

    if latest_build.status == running or latest_build.status == pending:
        return Building         # shouldn't normally happen if active_builds is consistent

    if latest_build.status == failed:
        return Failed           # last build of this commit failed

    if latest_build.status == success:
        if repo.has_pkgbase(pkgbase):
            return UpToDate     # built and in repo
        else:
            return Approved     # built but somehow not in repo (repo-add failed? manual removal?)

    return Approved             # fallback
```

### Status Values (V2)

```rust
enum DerivedStatus {
    PendingReview,      // never approved
    Approved,           // approved, not yet built (or build cancelled)
    Building,           // build in progress
    Failed,             // latest build of approved commit failed
    UpToDate,           // built, in repo, commit matches HEAD
    UpdateAvailable,    // HEAD moved past approved commit
    Missing,            // clone directory doesn't exist
}
```

Note: `Rejected` is removed. See Rejection Handling below.

### Rejection Handling

V1 had a `Rejected` status with a `rejection_reason` column. In V2, rejection is handled
differently: when a user rejects a commit, the `approved_commit` is simply not updated. The
package stays in `PendingReview` or `UpdateAvailable` state. If we want to record that a
specific commit was explicitly rejected (for audit), we can add a `rejections` table later,
but this is not needed for V2 MVP.

---

## Runtime Caches

### SrcInfoCache

In-memory cache of parsed .SRCINFO data from git clones.

```rust
struct SrcInfoCache {
    entries: HashMap<String, CachedSrcInfo>,  // pkgbase -> cached data
}

struct CachedSrcInfo {
    srcinfo: SrcInfo,    // full parsed .SRCINFO (existing parser)
    commit: String,      // commit hash this was parsed from
}
```

**Population**: Lazy -- populated on first access per package, or bulk-loaded on startup.
**Invalidation**: When a git fetch changes HEAD, invalidate that entry.
**Fallback**: If clone doesn't exist, entry is absent (status = Missing).

This provides:
- pkgbase -> pkgname mapping (from `srcinfo.packages`)
- Dependencies, provides, conflicts (from `srcinfo.depends`, per-package sections)
- Package version (`srcinfo.full_version()`)
- Package description (`srcinfo.pkgdesc`)

### RepoState

In-memory cache of the parsed repo.db.tar file.

```rust
struct RepoState {
    contents: RepoDbContents,   // from existing db_parser::parse_repo_db()
    last_modified: SystemTime,  // mtime of repo.db.tar file
}
```

**Population**: Parsed on startup.
**Invalidation**: Re-parsed after any repo-add/repo-remove operation, or when file mtime changes.

This provides:
- Which pkgnames are in the repo
- What version is in the repo for each pkgname
- Replaces `package_names.repo_version`

---

## AppState (V2)

```rust
struct AppState {
    // Persistent
    db: Database,
    config: Config,
    aur: AurClient,
    build: BuildSystem,
    repo: RepoManager,

    // Cached views of filesystem state
    srcinfo_cache: RwLock<SrcInfoCache>,
    repo_state: RwLock<RepoState>,

    // Transient build tracking
    active_builds: RwLock<HashSet<String>>,
    build_stages: RwLock<HashMap<String, BuildStage>>,
    build_cancel_flags: RwLock<HashMap<String, Arc<AtomicBool>>>,

    // Events + serialization
    events: broadcast::Sender<ServerEvent>,
    repo_mutex: Mutex<()>,          // serialize repo-add/repo-remove
}
```

Key change: No single `RwLock<AppState>` wrapping everything. Each piece of state has its own
lock. A build running doesn't block reading the package list.

---

## Code Organization (V2)

### New structure

```
src/
  api/
    mod.rs              # Route definitions, error types, extractors
    packages.rs         # Package CRUD handlers (thin)
    builds.rs           # Build trigger/status/cancel handlers (thin)
    repo.rs             # Repo info/clean/rebuild handlers (thin)
    system.rs           # System status, image, cache handlers (thin)
    events.rs           # SSE endpoint
  service/
    mod.rs
    packages.rs         # Package add/remove/approve/list logic
    builds.rs           # Build orchestration, queue, cancel
    updates.rs          # Update checking, auto-approve
    status.rs           # Status derivation logic
  state/
    mod.rs              # AppState definition
    srcinfo_cache.rs    # SrcInfoCache impl
    repo_state.rs       # RepoState impl
  db/
    mod.rs              # Database struct, migrations, queries
  models/
    mod.rs              # Package, Build, SrcInfo, etc.
  aur/
    mod.rs              # AUR RPC client, git operations, .SRCINFO parser
  build/
    mod.rs              # Podman container builds
  repo/
    mod.rs              # Repo file management
    db_parser.rs        # repo.db.tar parser (existing)
  config.rs             # Config loading
  main.rs               # CLI entry point
```

### What moves where

- Business logic currently in `api/mod.rs` -> `service/` modules
- API handlers become thin: parse request, call service, format response
- Status derivation -> `service/status.rs`
- Cache management -> `state/` modules

---

## Orphan Detection (V2)

No longer requires JSON parsing from DB. Uses SrcInfoCache directly.

```
find_orphans(packages, srcinfo_cache):
    explicit_deps = set()
    for pkg in packages where install_reason == explicit:
        srcinfo = srcinfo_cache.get(pkg.pkgbase)
        if srcinfo exists:
            for dep in srcinfo.all_depends():
                explicit_deps.add(strip_version(dep))

    orphans = []
    for pkg in packages where install_reason == dependency:
        srcinfo = srcinfo_cache.get(pkg.pkgbase)
        if srcinfo exists:
            pkgnames = srcinfo.packages.map(p -> p.pkgname)
            if none of pkgnames are in explicit_deps:
                orphans.push(pkg)
    return orphans
```

Same algorithm, but deps come from .SRCINFO instead of JSON columns.

---

## Migration Path

### Schema migration

1. Read all rows from V1 `packages` table
2. Create V2 `packages` table (can use ALTER TABLE DROP COLUMN for removed columns)
3. Drop `package_names` table
4. `builds` and `approvals` unchanged
5. On first startup, populate SrcInfoCache from existing git clones
6. Parse repo.db.tar for RepoState

### Data preservation

- Git clones: untouched
- Build logs: untouched
- Repo files (.pkg.tar.zst): untouched
- repo.db.tar: untouched
- Build history: untouched
- Approval history: untouched

### Handling missing clones

If a git clone doesn't exist for a tracked package (e.g. cache was cleared), the package
gets status `Missing`. The existing auto-re-clone logic (from commit 9ac5213) handles this:
on any operation that needs the clone, re-clone from AUR automatically.

---

## API Changes

### Response format changes

Package list/detail responses now include data from multiple sources instead of just DB:

```json
{
    "pkgbase": "example",
    "install_reason": "explicit",
    "auto_approve": false,
    "approved_commit": "abc123",
    "current_commit": "def456",      // from git clone HEAD
    "pkgver": "1.2.3-1",             // from .SRCINFO
    "status": "update_available",     // derived
    "pkgnames": [                     // from .SRCINFO
        {
            "pkgname": "example",
            "pkgdesc": "An example package",
            "depends": ["glibc", "openssl"],
            "repo_version": "1.2.2-1" // from repo.db.tar
        }
    ],
    "added_at": "...",
    "updated_at": "..."
}
```

### Endpoint changes

No endpoint URLs change. The data assembly happens in the service layer, transparent to
the frontend.

### Response normalization

V1 has several inconsistencies in API responses that the frontend works around. V2 fixes these:

**1. Consistent build object shape**

V1: Some endpoints return `{build: {...}, current_stage: "..."}`, others return the build
directly. The frontend has `const build = b.build || b;` workarounds.

V2: Build objects always have the same shape. `current_stage` is a field on the build object
itself (nullable), not a wrapper.

```json
{
    "id": "abc-123",
    "pkgbase": "example",
    "status": "running",
    "commit": "def456",
    "started_at": "...",
    "finished_at": null,
    "current_stage": "building",
    "error_message": null,
    "outputs": ["example-1.0-1-x86_64.pkg.tar.zst"],
    "log_path": "abc-123.log"
}
```

**2. `outputs` as real JSON array**

V1: `outputs` is stored as a JSON string in the DB and returned as a string in the API.
Frontend must `JSON.parse(build.outputs)`.

V2: `outputs` is returned as a proper JSON array (or null). No client-side parsing needed.

**3. PKGBUILD response always an object**

V1: Sometimes returns `{content: "..."}`, sometimes plain text. Frontend has
`result.content || result` fallback.

V2: Always returns `{content: "...", commit: "...", files: [...]}`.

**4. Deps inlined in package detail response**

V1: Frontend makes a separate `GET /api/packages/{name}/deps` call to show dependencies.

V2: The package detail response includes deps directly, assembled from .SRCINFO:

```json
{
    "pkgbase": "example",
    "...": "...",
    "dependencies": {
        "depends": [
            {"name": "glibc", "source": "official", "in_repo": false},
            {"name": "some-aur-dep", "source": "aur", "in_repo": true}
        ],
        "makedepends": [...],
        "checkdepends": [...],
        "optdepends": ["optional-thing: for extra features"]
    }
}
```

The separate `/api/packages/{name}/deps` endpoint is kept for backward compatibility but
the frontend no longer needs it.

**5. `is_orphan` flag on package list items**

V1: Orphans are a separate endpoint (`/packages/orphans`). Frontend has special-case code
to switch between the normal list and orphan list based on the filter dropdown.

V2: Each package in the list response includes `"is_orphan": true/false`. The frontend
filters orphans the same way it filters by status -- no separate endpoint needed.
`/packages/orphans` is kept for backward compatibility.

**6. `repo_version` included in package list response**

V1: List endpoint doesn't include repo versions. Must navigate to detail to see them.

V2: Each pkgname in the list response includes `repo_version` from repo.db.tar, enabling
"1.0 -> 1.1" update indicators in the list view without extra requests.

### SSE event improvements

**Full package object in status events**

V1: `package_status_changed` sends only `{pkgbase, new_status}`. Frontend must guess which
action buttons to show based on status string alone and does destructive DOM rebuilds.

V2: `package_status_changed` sends the full enriched package object (same shape as list items):

```json
{
    "type": "package_status_changed",
    "package": {
        "pkgbase": "example",
        "status": "up_to_date",
        "pkgver": "1.0-1",
        "is_orphan": false,
        "pkgnames": [...]
    }
}
```

This lets the frontend do a clean in-place update of the row without refetching or guessing.

---

## Implementation Sprints

### Sprint 1: Schema + State Layer

**Goal**: New DB schema, SrcInfoCache, RepoState. Compile and pass existing functionality.

Tasks:
- [ ] Write V2 migration in `db/mod.rs` (drop columns from packages, drop package_names)
- [ ] Update `Database` methods: remove all package_names queries, simplify package queries
- [ ] Update `Package` model: remove status, current_commit, pkgver, has_upstream, in_official_repos, rejection_reason
- [ ] Implement `SrcInfoCache` in `state/srcinfo_cache.rs`
- [ ] Implement `RepoState` in `state/repo_state.rs`
- [ ] Implement `derive_status()` in `service/status.rs`
- [ ] Update `AppState` to V2 structure (per-field locks)

### Sprint 2: Service Layer Extraction

**Goal**: Move business logic from api/mod.rs into service modules.

Tasks:
- [ ] Create `service/packages.rs`: add, remove, approve, list, orphan detection
- [ ] Create `service/builds.rs`: trigger, cancel, queue, recover stale
- [ ] Create `service/updates.rs`: check_updates, auto-approve
- [ ] Create `service/status.rs`: derive_status for single package and bulk
- [ ] Thin out `api/mod.rs` handlers to call service functions

### Sprint 3: API Handler Refactor + Response Normalization

**Goal**: Split api/mod.rs into focused modules, wire up to service layer, normalize responses.

Tasks:
- [ ] Create `api/packages.rs`, `api/builds.rs`, `api/repo.rs`, `api/system.rs`, `api/events.rs`
- [ ] Move route handlers to appropriate modules
- [ ] Normalize build response shape: always flat object, `current_stage` as nullable field
- [ ] Return `outputs` as real JSON array (not stringified)
- [ ] PKGBUILD endpoint always returns `{content, commit, files}` object
- [ ] Inline deps into package detail response (from SrcInfoCache)
- [ ] Add `is_orphan` boolean to package list items
- [ ] Add `repo_version` per-pkgname in list response (from RepoState)
- [ ] SSE `package_status_changed` sends full enriched package object
- [ ] Update CLI commands in main.rs to use new service/state layer

### Sprint 4: Frontend Updates

**Goal**: Update frontend to use normalized responses, remove workarounds.

Tasks:
- [ ] Remove `const build = b.build || b` workarounds (consistent build shape)
- [ ] Remove `JSON.parse(build.outputs)` -- use array directly
- [ ] Remove `result.content || result` fallback for PKGBUILD
- [ ] Use inlined deps from detail response instead of separate `/deps` fetch
- [ ] Use `is_orphan` field for orphan filtering instead of separate endpoint
- [ ] Use `repo_version` in package list for version comparison display
- [ ] Handle enriched SSE `package_status_changed` events (in-place row update)
- [ ] Remove dead code paths for old response formats

### Sprint 5: Verification + Cleanup

**Goal**: Verify all existing functionality works, update CLAUDE.md and DESIGN.md.

Tasks:
- [ ] Test all API endpoints manually (package add, list, approve, build, remove)
- [ ] Test CLI commands (add, list, build, check-updates, etc.)
- [ ] Test frontend (dashboard, package list, detail, build log viewer)
- [ ] Test edge cases: missing clones, empty repo, split packages, orphan detection
- [ ] Update CLAUDE.md with V2 architecture notes
- [ ] Update DESIGN.md schema section
- [ ] Remove dead code

---

## Verification Criteria

These must all pass after implementation:

### Functional parity
- [ ] `POST /api/packages` adds a package (clone, parse .SRCINFO, track in DB)
- [ ] `GET /api/packages` returns package list with status, pkgnames, deps, repo_version
- [ ] `GET /api/packages/{name}` returns full detail including .SRCINFO data and build history
- [ ] `POST /api/packages/{name}/approve` stores approved_commit, status becomes Approved
- [ ] `POST /api/packages/{name}/build` triggers containerized build
- [ ] Build success: package in repo, status UpToDate, repo.db.tar updated
- [ ] Build failure: status Failed, error_message in build record
- [ ] `POST /api/packages/check-updates` detects new commits, auto-approves where configured
- [ ] `DELETE /api/packages/{name}` removes package, cascades correctly
- [ ] `GET /api/packages/orphans` returns correct orphan list
- [ ] Frontend renders all pages without errors
- [ ] CLI commands produce correct output

### Derived status correctness
- [ ] New package (never approved) -> PendingReview
- [ ] Approved but not built -> Approved
- [ ] Built successfully, in repo -> UpToDate
- [ ] Build failed -> Failed
- [ ] HEAD moved past approved_commit -> UpdateAvailable
- [ ] Build in progress -> Building
- [ ] Clone missing -> Missing (triggers re-clone on next operation)

### Cache correctness
- [ ] SrcInfoCache populated from git clones on startup
- [ ] SrcInfoCache invalidated after git fetch changes HEAD
- [ ] RepoState re-parsed after repo-add/repo-remove
- [ ] Stale cache doesn't cause incorrect status

### Response normalization
- [ ] Build objects never wrapped -- always flat with nullable `current_stage`
- [ ] `outputs` returned as JSON array, not stringified JSON
- [ ] PKGBUILD endpoint returns `{content, commit, files}` always
- [ ] Package detail includes `dependencies` object (depends, makedepends, checkdepends, optdepends)
- [ ] Package list items include `is_orphan` boolean
- [ ] Package list pkgnames include `repo_version`
- [ ] SSE `package_status_changed` includes full package object

### Frontend
- [ ] No `JSON.parse(build.outputs)` calls remain
- [ ] No `b.build || b` workarounds remain
- [ ] No `result.content || result` workarounds remain
- [ ] Orphan filter uses `is_orphan` field, not separate endpoint
- [ ] Detail page deps come from inline response, not separate fetch
- [ ] SSE status events update rows in-place

### No regressions
- [ ] SSE events still fire for builds, update checks
- [ ] Build log streaming still works (WebSocket)
- [ ] Build cancellation still works
- [ ] Dependency-ordered build queue still works
- [ ] Source cache (SRCDEST) still works
- [ ] Old build logs still accessible
