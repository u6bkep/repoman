# Sprint 3: Live Status & UX Improvements

## Problem Statement

Two core user stories have significant friction:

1. **Periodic updates**: Requires navigating between 2 pages, a blocking wait with no progress feedback, and no auto-refresh. The `check_updates` endpoint does sequential `git fetch` per package (slow with many packages).

2. **Adding packages with deps**: Silent failure when base image is missing, dep checkbox defaults to off, no post-add summary, and no way to approve+build the newly added set without navigating to the dashboard.

Cross-cutting: No live status updates anywhere — the UI is a static snapshot that requires manual refresh.

## Architecture Decision: SSE Event Bus

Single `GET /api/events` SSE endpoint backed by `tokio::sync::broadcast<ServerEvent>`.

**Event contract** (all branches should code against this shape):
```rust
// Backend enum
enum ServerEvent {
    PackageStatusChanged { pkgbase: String, new_status: String },
    BuildStarted { build_id: String, pkgbase: String },
    BuildStageChanged { build_id: String, pkgbase: String, stage: String },
    BuildCompleted { build_id: String, pkgbase: String, status: String },
    CheckUpdatesProgress { checked: usize, total: usize, current: String },
    CheckUpdatesComplete { checked: usize, updated: Vec<String> },
}
```

```js
// Frontend: any component can listen without importing anything
// Events dispatched as CustomEvent on window by events.js
window.addEventListener('repoman:status', (e) => {
    const { type, ...data } = e.detail;
    // type = "package_status_changed" | "build_started" | etc.
});
```

SSE data frames use JSON with a `type` discriminator field, same pattern as the existing WebSocket stage messages.

---

## Branch 1: `sprint-3/sse-event-bus` — Backend Infrastructure

**Touches**: `src/api/mod.rs`, `Cargo.toml`

### Tasks

1. **Add broadcast channel to AppState**
   - Add `events: tokio::sync::broadcast::Sender<ServerEvent>` field to `AppState` (line 24)
   - Create with `broadcast::channel(256)` during server startup
   - Define `ServerEvent` enum (serializable with serde, tagged via `#[serde(tag = "type")]`)

2. **Add SSE endpoint**
   - `GET /api/events` → Axum `Sse` response
   - Subscribe via `state.read().await.events.subscribe()`
   - Convert each `ServerEvent` to `axum::response::sse::Event` with JSON data
   - Handle lagged receivers gracefully (skip missed events, don't disconnect)
   - No auth needed (same as existing endpoints)

3. **Emit events at mutation points**
   - `set_build_stage()` (line 1154): emit `BuildStageChanged`
   - `run_build_task()` success/failure paths (lines 1237, 1350): emit `BuildCompleted` + `PackageStatusChanged`
   - `run_queued_builds()` loop start (line 2346): emit `BuildStarted`
   - `run_queued_builds()` skip path (line 2324): emit `BuildCompleted { status: "skipped" }`
   - `approve_and_build_updates()` (line 1001): emit `PackageStatusChanged` for each approved package
   - `add_package()` (line 522): emit `PackageStatusChanged` for each newly added package
   - `check_updates()` loop (line 1618): emit `CheckUpdatesProgress` per package, `CheckUpdatesComplete` at end

4. **Parallelize check_updates**
   - Replace the sequential `for pkg in packages` loop (line 1618) with concurrent fetches
   - Use `tokio::task::JoinSet` with a semaphore (limit ~8 concurrent git fetches)
   - `AurGit::has_updates` and `AurGit::fetch` are blocking (`Command::new("git")`) — wrap each in `spawn_blocking`
   - Still emit `CheckUpdatesProgress` as each package completes (total + current count)
   - Still acquire write lock briefly per-package for DB updates (same as now)

5. **Add combined check-and-build endpoint**
   - `POST /api/packages/check-and-build` — runs check_updates logic, then approve_and_build logic
   - Returns `CheckAndBuildResponse { checked, updated, approved, build_order, build_ids, skipped }`
   - This gives the dashboard a true one-click path

6. **Validate base image in add_package when with_deps=true**
   - In `add_package()`, before attempting dep detection, check `state.build.image_exists()`
   - If missing and `with_deps=true`: return HTTP 400 with clear error message: "Base container image required for dependency detection. Run rebuild-image first."
   - Currently this silently returns empty `missing_deps` (bad UX)

### Cargo.toml changes
- Axum already has `ws` feature; SSE is in axum core (no feature flag needed)
- `tokio-stream` may be needed for the SSE stream adapter — add if not already present

### Testing
- `cargo check` must pass
- Manual: `curl -N http://localhost:8088/api/events` should stream events when triggering actions in another terminal

---

## Branch 2: `sprint-3/frontend-events-dashboard` — Event System + Dashboard UX

**Touches**: `static/js/events.js` (new), `static/js/main.js`, `static/js/api.js`, `static/js/components/dashboard.js`, `static/index.html` (dashboard template only)

### Tasks

1. **Create `static/js/events.js`** — global event manager
   ```js
   // Connects to /api/events via EventSource
   // Dispatches typed CustomEvents on window
   // Auto-reconnects (EventSource does this natively)
   // Exports connect() and disconnect() for lifecycle
   ```
   - Parse SSE `data:` as JSON
   - Dispatch as `window.dispatchEvent(new CustomEvent('repoman:status', { detail: parsed }))`
   - Log connection state to console (connected, reconnecting)
   - Graceful degradation: if SSE endpoint doesn't exist yet, just silently retry

2. **Wire into `main.js`**
   - Import and call `events.connect()` at startup (after router init)
   - This is fire-and-forget — components opt in to events via `window.addEventListener`

3. **Add to `api.js`**
   - `system.checkAndBuild: () => post('/packages/check-and-build')`

4. **Rework dashboard (`dashboard.js`)**
   - **Add "Check & Build Updates" button** to the dashboard header area (not buried in attention list)
     - This calls the new combined endpoint
     - Shows progress: "Checking 12/50..." while check runs (via SSE events)
     - On complete: shows result toast and refreshes attention list
   - **Add "Check Updates" as secondary action** (for users who want to review before building)
   - **Wire up event listeners**:
     - On `package_status_changed` / `build_completed`: reload attention list
     - On `check_updates_progress`: update progress indicator
   - **Recent Builds section**: The `<section class="recent-builds">` template exists (line 45-50) but is never populated — wire it up to show last ~5 builds from `api.builds.list()` or a new recent-builds endpoint
   - Cleanup: return a cleanup function that removes event listeners (same pattern as build-log.js)

5. **Update `index.html` dashboard template**
   - Add a header action area for the check/build buttons (similar to package-list page header)
   - Keep existing status cards and attention sections

### Event listener pattern (for all components)
```js
// In component init():
const handler = (e) => {
    if (e.detail.type === 'build_completed') reloadData();
};
window.addEventListener('repoman:status', handler);
// Return cleanup:
return () => window.removeEventListener('repoman:status', handler);
```

---

## Branch 3: `sprint-3/add-package-ux` — Add Package Flow

**Touches**: `static/js/components/add-package.js`, `static/js/components/package-detail.js`, `static/index.html` (add-package and package-detail templates only)

### Tasks

1. **Default "with deps" checkbox to checked**
   - `index.html` line 156: add `checked` attribute to `<input type="checkbox" id="add-with-deps">`
   - This matches the common case for a large repo — you almost always want deps resolved

2. **Post-add summary before navigation**
   - After successful add with deps, instead of immediately navigating away, show an inline summary panel in the add-package page:
     ```
     ✓ Added my-app and 5 dependencies

     Package          Version    Status
     my-app           1.0-1      Pending Review
     lib-a            0.5-1      Pending Review (dep)
     lib-b            1.2-1      Pending Review (dep)
     ...

     [Approve & Build All]  [View my-app]
     ```
   - The "Approve & Build All" button calls `api.packages.approveAndBuild()` — same endpoint the dashboard uses
   - "View" navigates to the package detail page
   - If added WITHOUT deps and there are missing deps, show a persistent warning panel:
     ```
     ⚠ Missing AUR dependencies: pkg1, pkg2, pkg3
     [Add Missing Dependencies]
     ```
   - The "Add Missing Dependencies" button re-adds the same package with `withDeps: true` (the endpoint is idempotent — it'll skip the already-added package and add the missing deps)

3. **Package detail: "Approve & Build with Deps" context action**
   - On the package detail page, when a package is `pending_review` and has tracked dependencies also in `pending_review`, show a contextual button: "Approve & Build All Pending"
   - This is the same `approveAndBuild()` call — but having it here saves the trip to the dashboard after adding a package

4. **Package detail: missing deps banner**
   - When viewing a package whose deps tab shows untracked AUR dependencies, show a persistent info banner above the tabs:
     ```
     ℹ This package has 2 untracked AUR dependencies
     [Add Missing Dependencies]
     ```
   - Button calls `api.packages.add(pkgbase, { withDeps: true })` for the current package
   - On success, refresh the page to show the newly added deps

---

## Branch 4: `sprint-3/package-list-ux` — Package List Search + Live Updates

**Touches**: `static/js/components/package-list.js`, `static/index.html` (package-list template only)

### Tasks

1. **Add text search filter**
   - Add a search input to the filter bar in the package-list template (next to the status/reason dropdowns)
   - Filter client-side: `packages.filter(pkg => pkg.pkgbase.includes(query.toLowerCase()))`
   - Debounce input (200ms) to avoid re-rendering on every keystroke
   - Clear button (×) to reset search

2. **Live status updates via events**
   - Listen for `repoman:status` events
   - On `package_status_changed`: update the status badge for the affected package in-place (without full re-render)
   - On `build_started` / `build_completed`: same — update the specific row
   - This avoids the jarring full-reload that resets scroll position

3. **Update `index.html` package-list template**
   - Add search input to the filters div (line 58-76)

---

## File Ownership Matrix

| File | Branch 1 | Branch 2 | Branch 3 | Branch 4 |
|------|----------|----------|----------|----------|
| `src/api/mod.rs` | ✅ | | | |
| `Cargo.toml` | ✅ | | | |
| `static/js/events.js` | | ✅ (new) | | |
| `static/js/main.js` | | ✅ | | |
| `static/js/api.js` | | ✅ | | |
| `static/js/components/dashboard.js` | | ✅ | | |
| `static/js/components/add-package.js` | | | ✅ | |
| `static/js/components/package-detail.js` | | | ✅ | |
| `static/js/components/package-list.js` | | | | ✅ |
| `static/index.html` | | tmpl-dashboard | tmpl-add-package, tmpl-package-detail | tmpl-package-list |
| `static/js/components/utils.js` | | | | |

**Conflict risk**: `static/index.html` is touched by branches 2, 3, and 4, but each modifies a different `<template>` block. Merges should be clean. No other file conflicts.

---

## Merge Order

All branches can be developed in parallel. Merge order:

1. **Branch 1** first (backend — everything else benefits from SSE being live)
2. **Branches 2, 3, 4** in any order (no dependencies between them)

After merge: the event listeners in branches 2/3/4 will start receiving live events from branch 1's SSE endpoint. Before branch 1 merges, those listeners simply never fire (graceful degradation).

---

## Out of Scope (Future)

- WebSocket replacement (keep existing per-build log WebSocket as-is)
- Pagination for package list (not needed until 500+ packages)
- Bulk PKGBUILD review interface (multi-diff viewer)
- Dependency graph visualization
- Build queue status view
