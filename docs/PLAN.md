# repoman v2 — Design and Implementation Plan

Status: proposed, 2026-10-31, revision 2 after fresh-context review. Written
from a clean-room brainstorm; v1 code was not consulted except to measure
build timing from its logs.

## 1. Purpose

repoman builds AUR packages for one person's machines and publishes them as
signed pacman repositories. v2 is a rewrite. It keeps the core idea and adds:

- Isolated, instrumented, network-off builds.
- Concurrent builds across several worker machines.
- Release streams so machines on different Manjaro branches each get packages
  built against their own libraries.
- A review gate with heuristics plus LLM-assisted diff review, so updates are
  neither rubber-stamped nor blindly trusted.
- A pull-through package cache shared by builders and clients.
- A web UI for the review queue, runs, and history.

### Non-goals

- Unattended operation. Every pipeline run is user-triggered. The daemon runs
  continuously only to serve the UI, workers, and the cache.
- aarch64 or any non-x86_64 target. Architecture is modelled, not implemented.
- A general AUR helper for other people. The tool is open source, but the
  design assumes one operator.
- Analysing upstream source tarballs or binaries for malice. The gate reviews
  what the AUR maintainer controls and corroborates what upstream serves.
- Deciding binary compatibility across streams. Every stream gets its own
  build. See the ledger for why.

## 2. Measured facts that shaped the design

From v1's 414 logs and 427 build records, 2026-01 to 2026-09:

| Measure, successful builds           | p50  | p90  | max  |
|--------------------------------------|------|------|------|
| makepkg wall time, seconds           | 13   | 67   | 845  |
| ... builds that installed deps       | 14   | 82   | 845  |
| ... builds with no dep install       | 3    | 28   |      |
| Installed dep size per build, MiB    | 471  | 903  | 2469 |
| Queue wait before makepkg, seconds   | 979  | 2497 | 4960 |

Total makepkg time over nine months: 3.7 hours. Total queue wait: 113 hours.
Serial scheduling, not dependency extraction, was the bottleneck. Build
scratch directories left 19 GB on disk against 38 MB of logs. One failure was
scratch disk exhaustion.

Consequences: plain chroot-plus-shared-cache provisioning is sufficient;
concurrency, per-job resource limits, and retention are first-order
requirements; building every stream separately costs hours per year and is
affordable; per-package layered filesystems are deferred behind an interface.

## 3. Decisions ledger

Each entry records what was chosen and what was rejected, so a future reader
can reopen a decision on evidence rather than drift.

| Area | Chosen | Rejected, and why |
|------|--------|-------------------|
| Streams and repos | One repo per stream. Every member package is built on every member stream. `arch=any` packages without a versioned-interpreter dependency are built once and the same file is published to every member stream's repo via the content store. | A shared `common` repo with per-stream fallbacks, decided by an ELF soname scan (proposed 2026-10-31, reversed same day). Rejected because: the soname check is necessary but not sufficient (Qt/KDE private ABI, plugin dirs, Go/Haskell hashed interfaces break with no soname change, so a false `common` verdict fails silently on clients); pacman resolves a name to the first listed repo regardless of version, so layout flips need `repo-remove` bookkeeping or stale entries shadow forever; identically named per-stream builds collide in a single staging repo or filename-keyed cache; the saving was 3.7 build-hours per nine months on machines that are now concurrent. The ELF scan survives for a sound use: soname-bump rebuild triggers. |
| Build root provisioning | Per-stream immutable base image, snapshotted per build via overlayfs; deps installed by pacman from the shared cache. Provisioner is a trait. | Per-package overlay layers: 500-layer kernel cap, lookup cost, hook replay complexity. composefs/erofs content-addressed roots: elegant, but measured dep install is 10–60 s and not the bottleneck. Revisit if dep sets or build frequency grow 10x. |
| Base image construction | Built by a worker advertising the `image` capability (root with `pacstrap`), packed as a content-addressed tarball, uploaded to the coordinator, and downloaded by other workers into a fresh directory per image. Never mutated in place. (Revised 2026-10-31 during M1: the plan first said the coordinator container would run pacstrap, but pacstrap needs mounts and the NAS container is unprivileged.) | `pacman -Syu` inside the image on the worker: mutating the lower layer of an active overlay is undefined behaviour. Building in the coordinator container: needs a privileged container on the NAS. |
| Isolation primitives | Own executor on `unshare`, bind mounts, overlayfs, network namespace, one cgroup per job. Worker runs as root under systemd hardening; makepkg runs as an unprivileged uid inside for every phase, since makepkg refuses root in all modes. | devtools `makechrootpkg`: cannot give a network-off build phase, upper-dir inspection, or per-stage timing without fighting it. Docker/podman: moves root to a daemon we don't control and blocks the same introspection. |
| Worker transport | Worker opens a persistent WebSocket to the coordinator and holds it open. Token auth. | Coordinator-driven SSH: fragile for long jobs, no capability advertisement. HTTP polling: pointless with two or three workers. |
| Coordinator location | Container on the NAS (Ubuntu docker host, always on, hosts the repos and reverse proxy). Arch-based image so `pacstrap`, `repo-add`, `gpg`, `bsdtar` are present. | Coordinator on the desktop: v1 ran there only because it builds well; the desktop is now a worker. Publishing over NFS: removed a failure class. |
| Artifact storage | Content-addressed store on the NAS; repo URLs resolve to store objects through the publication table at serve time. | Plain directories with copies per repo: duplicates `any` packages. Hardlinks or reflinks: workable fallback for an external static server, but DB-driven serving gives retention and dedupe for free. |
| State store | SQLite on the NAS SSD, single writer in the coordinator, append-only event table. | Postgres: unnecessary for one operator. |
| Trigger model | User-triggered "update run". No cron, no scheduler. | Unattended polling: operator explicitly does not want it. |
| LLM backend | Direct Claude API over HTTPS from Rust with structured outputs. Default model `claude-opus-5-5`; `claude-sonnet-5-5` for bulk routine triage. API credits bundled with the Max plan fund it. | Shelling out to `claude -p` with subscription login: works, tested, but sits in a gray zone of the Claude Code licence once the tool is published for others. |
| Review authority | Heuristics compute a floor. The LLM may raise risk, never lower it. Structural changes always wait for the human. | LLM as sole gate: prompt-injectable via PKGBUILD content. Human reviews everything: becomes a rubber stamp. |
| Quarantine clock | AUR server-side `LastModified` as first observed by repoman, stored on the candidate. | Git commit date: the pusher controls it and can backdate past the quarantine window. |
| Baseline advance | On publish. | On approval: a post-build rejection or a failed build would leave the baseline at an unpublished commit and the package would never be re-candidated. |
| Scheduling | M1: fixed slots per worker plus per-job cgroup memory limit and scratch quota; peak RAM, scratch, CPU, and wall recorded per job. M4: admission by predicted footprint from recorded peaks, with limits set from the prediction and a retry-alone-with-double-limit on breach. | Fixed slots forever: tuned for the worst coincidence, so two or three on a machine that could run ten typical builds. Full resource scheduler with preemption: unjustified. |
| Architecture | `arch` is a column on builders, jobs, artifacts, repos. Only `x86_64` implemented. | Implementing aarch64 speculatively: emulation is slow and flaky, no ARM machine exists. Omitting the field: retrofit touches everything. |
| Package cache | Built-in `cache` role of the same binary, running on the NAS, for upstream repos only. Eviction uses sync-DB knowledge. | nginx `proxy_store` or pacoloco: age-based eviction only, second service. Caching repoman's own repos: same filename, different bytes per stream; served directly instead. |
| UI | Server-rendered HTML with light interactivity, plus a CLI. | SPA, TUI. |
| Onboarding | One-time curated wanted list produced in a working session, assigned to M1. | Persistent client-reporting agent and audit metadata: overbuilt. |
| Network-attempt detection | M1: loopback-only namespace, attempts simply fail. M2: nftables counter and log rule in the build namespace so attempts are recorded and become a gate input. | Claiming detection from a bare namespace: it blocks but records nothing. |

## 4. Topology

```
 NAS (b-srv0, Ubuntu, docker)             Desktop (Manjaro unstable)
 ┌────────────────────────────────┐       ┌──────────────────────────┐
 │ repoman coordinator (container)│◄──WS──│ repoman worker (systemd) │
 │  - SQLite + GPG home on SSD    │       │  - base images, overlay  │
 │  - UI + API  (behind proxy)    │       │  - N concurrent builds   │
 │  - worker hub                  │       └──────────────────────────┘
 │  - gate, planner, publisher    │
 │  - base image builder          │◄──WS──  Laptop worker (optional)
 │  - artifact store + repos (HDD)│
 │  - cache role (HDD)            │
 └────────────────────────────────┘
         ▲ https (reverse proxy)
         │ pacman: Server = repos/<stream>, CacheServer = cache (upstream only)
   Desktop + Laptop as pacman clients
```

Workers need a Linux kernel with namespaces, overlayfs, and cgroup v2. The
build root contains pacman and base images arrive as tarballs, so a worker
host need not run Arch. The NAS itself could run a worker later.

## 5. Components

One Rust binary, `repoman`, with subcommands selecting a role:

- `repoman coordinator` — everything stateful.
- `repoman worker` — connects to a coordinator, executes jobs.
- `repoman cache` — may run inside the coordinator process or standalone.
- `repoman <cli verbs>` — talks to the coordinator API.

### 5.1 Coordinator modules

| Module | Responsibility |
|--------|----------------|
| `store` | SQLite schema, migrations, typed queries, event log. |
| `api` | HTTP: JSON API, server-rendered UI, artifact upload, repo serving from the content store, staging repo serving, base image serving, cache serving. |
| `hub` | WebSocket server for workers: registration, capabilities, job dispatch, log and stage streaming, heartbeats, reconnection, job state idempotency. |
| `aur` | RPC multiinfo, optional bulk metadata fetch, git clone/fetch of package bases, SRCINFO parsing, commit history, provider lookup by `provides`. |
| `streams` | Per-stream config, sync-DB fetch and parse (`.db` and `.files`), provides and version index, version comparison. |
| `images` | Base image build per stream via `pacstrap`, tarball packing, hashing, serving, GC. |
| `planner` | Dependency DAG across AUR and official packages per stream, discovery of new AUR dependencies, build order, job generation, scheduling to workers. |
| `gate` | Provenance class, diff classification, heuristic lint, signals, corroboration, LLM reviewer, verdict floor, candidate lifecycle, review queue. |
| `keys` | PGP key cache for `validpgpkeys`, keyserver and WKD fetch, per-package key policy. |
| `publisher` | Staging repos per run and stream, signing, `repo-add`, atomic publish, retention over the content store. |
| `elfscan` | Post-build NEEDED soname extraction stored per artifact; input to M4 rebuild triggers. |
| `cache` | Pull-through upstream package cache with verification and eviction. |

### 5.2 Worker modules

| Module | Responsibility |
|--------|----------------|
| `link` | WebSocket client, capability report, job intake, backpressure. |
| `images` | Download and unpack base images by hash into immutable directories; GC images no job references. |
| `executor` | Job types `build`, `inspect`, `probe`. Namespaces, overlay, cgroup, phases, timing, upper-dir diff, resource peaks, artifact collection. |
| `upload` | Content-addressed artifact and log upload to the coordinator. |

## 6. Data model

SQLite tables, abbreviated. All timestamps UTC. `id` columns are ULIDs.

```
stream         (name PK, distro, branch, mirror_url, keyring_pkg, repos JSON,
                arch, sort_order)
package        (pkgbase PK, provenance_class, policy JSON, kind, added_at)
                -- kind: release | vcs
                -- policy keys: pgp = "skip" (sources without a fetchable key),
                --   allow_network_build = true (prepare() fetches at build time;
                --   found on pico-sdk), build_once = true|false (override the
                --   arch=any rule), provider_choices = {dep: pkgname},
                --   quarantine_days, auto_metadata
package_stream (pkgbase, stream, wanted BOOL, required BOOL)
baseline       (pkgbase PK, aur_commit, srcinfo_hash, published_at, note)
aur_snapshot   (pkgbase, fetched_at, last_modified, maintainer, submitter,
                comaintainers JSON, out_of_date, votes, popularity,
                head_commit)
run            (id PK, started_at, finished_at, state, summary JSON)
                -- state: running | needs_input | building | publishing |
                --        done | aborted
candidate      (id PK, run_id, pkgbase, kind, aur_commit, upstream_rev,
                first_seen_last_modified, first_seen_at, class, floor_risk,
                llm_risk, state, decided_at, decided_by, review JSON)
                -- kind: update | vcs | onboarding | dependency_rebuild |
                --       soname_rebuild
                -- state: pending | approved | rejected | deferred | building |
                --        built | publish_review | published | failed
signal         (id PK, pkgbase, aur_commit, source, severity, payload JSON,
                observed_at)
corroboration  (candidate_id, check, result, detail JSON)
job            (id PK, run_id, candidate_id, kind, pkgbase, stream,
                aur_commit, base_image, arch, worker, state, queued_at,
                started_at, finished_at, exit_code, error,
                peak_rss_bytes, peak_scratch_bytes, cpu_seconds,
                net_attempts, outside_writes JSON)
job_stage      (job_id, stage, started_at, finished_at)
job_log        (job_id, seq, ts, fd, line)         -- or a file per job
artifact       (sha256 PK, job_id, pkgbase, pkgname, pkgver, arch, stream,
                filename, size, sig_sha256, buildinfo JSON, needed JSON,
                provides JSON, files_summary JSON, upstream_rev)
publication    (artifact_sha, repo, published_at, retired_at)
base_image     (id PK, stream, created_at, package_list_hash, sha256, size)
cache_object   (filename PK, repo, size, sha256, first_seen, last_served,
                referenced BOOL, unreferenced_since)
event          (seq PK, ts, kind, subject, payload JSON)
worker         (name PK, token_hash, arch, slots, mem_bytes, scratch_bytes,
                last_seen)
```

Artifacts and their detached signatures live in the content store under
their sha256. Repo directories hold only the database files; package URLs are
resolved through `publication` at serve time.

## 7. The update run

Triggered from the UI or `repoman update`. The run is a loop over steps 1–8
until no new candidates appear, then 9–12. Each step is resumable; an
interrupted run shows where it stopped.

1. **Refresh streams.** Fetch each stream's sync databases if the mirror's
   `Last-Modified` moved. Parse into the provides and version index.
   Rebuild base images older than a day (section 8.1).
2. **Poll AUR.** One RPC multiinfo call for every pkgbase in `package`.
   Compare `LastModified`, `Maintainer`, and co-maintainers with the stored
   snapshot. For `vcs` packages, enqueue `probe` jobs that run `git ls-remote`
   against the source and compare with the latest published artifact's
   `upstream_rev` per stream.
3. **Fetch changed repos.** `git fetch` for pkgbases whose `LastModified`
   moved. Record head commit and the commits since the baseline.
4. **Create candidates.** For each pkgbase whose head differs from the
   baseline and has no live candidate for that commit: an `update` candidate
   with `first_seen_last_modified` from the snapshot. For each VCS head move:
   a `vcs` candidate. For each pkgbase with no baseline: an `onboarding`
   candidate. Candidates approved in earlier runs but not yet published are
   carried forward, not recreated. A `rejected` candidate stays rejected
   until the head moves.
5. **Classify.** For `update` candidates: parse baseline and head SRCINFO,
   run the diff classifier and heuristic lint, compute the floor risk, and
   enqueue an `inspect` job that regenerates SRCINFO from the PKGBUILD in a
   sandbox and compares. `vcs` candidates skip classification: the AUR
   content is unchanged; they are gated by build evidence and artifact diff.
6. **Signals and corroboration.** Query the aur-security lookup API for
   candidate commits; record AUR state changes; run forge adapters for
   routine updates. Store in `signal` and `corroboration`. Signals persist
   across runs.
7. **LLM review.** For candidates above the routine class, or flagged by any
   signal, call the API reviewer. Store prompt, response, model, and cost.
8. **Queue.** Present candidates in three groups: auto-approved routine
   changes past quarantine, routine changes inside quarantine, and everything
   needing a decision. Onboarding candidates require a provenance class and a
   stream membership. The operator approves, rejects, or defers. If the
   planner (step 9) discovered new AUR dependencies, the run returns to step 2
   for them and the dependents wait in `needs_input`.
9. **Plan.** For approved candidates, resolve depends, makedepends, and
   checkdepends per member stream against the stream index and the set of
   AUR packages. Unknown AUR dependencies are inserted into `package` as
   `required` and cause a loop. Virtual depends with several AUR providers
   require an operator choice recorded in policy. Build the per-stream DAG
   and emit `build` jobs in topological order. `any` packages without a
   versioned-interpreter dependency get one job on the lowest-sort member
   stream. Dependents of a rebuilt AUR package get `dependency_rebuild`
   candidates, auto-approved, since their AUR content is unchanged.
10. **Build.** Dispatch to workers within slots and limits. Each successful
    artifact is uploaded to the content store and added to the run's staging
    repo for that stream so dependents can build against it. A failed job
    retries once on a fresh root; a second failure marks the candidate
    `failed`. Failed candidates stay approved and are re-planned next run
    until they succeed or a newer head supersedes them.
11. **Post-build review and publish.** Run `elfscan` and the artifact diff
    against the previous publication. A network attempt count above zero
    (M2), writes outside expected paths, or new privileged surface moves the
    candidate to `publish_review` for the operator. Otherwise sign, add to
    each member stream's repo, write the databases atomically, record
    publications, advance the baseline, retire superseded publications, apply
    retention.
12. **Report.** Summarise the run: built, published, awaiting review, failed,
    skipped, cost.

## 8. Executor

### 8.1 Base images

An `image` job on a capable worker builds one image per stream: `pacstrap -C
<stream pacman.conf> -G -M -K` into a directory with `base` and `base-devel`,
populates the stream's keyrings, creates the build user, writes the repoman
makepkg.conf.d drop-in, then packs it as a zstd tarball named by sha256 and
uploads it to the coordinator's blob store. The worker host's pacman keyring
(or `pacstrap_gpgdir`) must trust the stream's packages. A new image
is built when the stream's databases change and the current image is older
than a day. Workers download images by hash into `images/<sha256>/` and never
modify them. Images with no in-flight job and a newer successor are deleted
on both sides.

### 8.2 Build job phases

Every phase boundary emits a timestamped stage event. Every log line is
timestamped at the worker before upload. The job runs in its own cgroup with
a memory limit and a scratch quota; peaks are read from the cgroup and the
upper directory at teardown.

1. **provision** — mount overlayfs with the image as lower and a fresh upper
   on the worker's scratch disk. Bind the shared `SRCDEST`, owned by the build
   uid. Write the root's `pacman.conf`: the stream's repos with `CacheServer`
   pointing at the cache role and `Server` at the mirror, plus the run's
   staging repo for this stream served by the coordinator over HTTP with
   `SigLevel = Never` for that repo only. Create the build uid and a per-job
   `GNUPGHOME` containing the keys from the package's `validpgpkeys`,
   supplied by the coordinator's key cache.
2. **deps** — `pacman -S --needed` for the resolved depends, makedepends,
   and checkdepends. Network on.
3. **sources** — `makepkg --verifysource` as the build uid. Downloads into
   `SRCDEST`, checks checksums and PGP signatures, exits. Network on. For
   `vcs` packages this updates the mirror clone in `SRCDEST`.
4. **build** — enter a network namespace with loopback only (M2: plus an
   nftables rule that counts and logs any non-loopback egress), unless the
   package's policy sets `allow_network_build`, which the log records; then run
   `makepkg --holdver --noconfirm` as the build uid. Sources are already in
   `SRCDEST`, so extraction, `prepare()`, `pkgver()`, `build()`, `check()`,
   and `package()` all run offline. `--holdver` prevents the VCS re-fetch
   that would otherwise abort. A `prepare()` that fetches at build time fails
   here and is recorded.
5. **collect** — gather `*.pkg.tar.*`, `.BUILDINFO`, logs, cgroup peaks, net
   counters. Compute the upper-directory diff and record paths written
   outside `srcdir`, `pkgdir`, `SRCDEST`, and pacman's own directories.
6. **teardown** — unmount, delete the upper. Scratch survives only on failure
   and only until the retention limit.

Timeouts apply per job. On a memory-limit kill or quota breach the retry runs
alone with the limit doubled.

### 8.3 Inspect and probe jobs

`inspect` runs `makepkg --printsrcinfo` as the build uid inside a network-off
sandbox and returns the generated SRCINFO. `probe` runs `git ls-remote` for
VCS sources with network on. Both are cheap and run on any worker.

### 8.4 PGP keys

makepkg does not fetch `validpgpkeys`. The coordinator keeps a key cache by
fingerprint, filling it during candidate classification via keyserver and
WKD lookups with network, and ships the needed public keys to the worker for
the job's `GNUPGHOME`. A package whose keys cannot be found anywhere needs an
operator decision: supply the key manually or set `pgp = skip` in its policy,
which is itself shown in the review UI. A change to `validpgpkeys` is always
structural.

## 9. Streams

### 9.1 Definition

A stream is a distro, branch, mirror, keyring package, repo list, and sort
order. Initial streams: `manjaro-stable` and `manjaro-unstable`. Each client
machine belongs to one stream. A package's membership defaults to all streams
and is editable; a package excluded from a stream never appears in that
stream's repo.

### 9.2 Repo layout and client config

```
repos/manjaro-stable/x86_64/manjaro-stable.{db,files}.tar.zst{,.sig}
repos/manjaro-unstable/x86_64/manjaro-unstable.{db,files}.tar.zst{,.sig}
store/<sha256>/<filename>            -- artifacts and .sig files
```

Package requests to `repos/<stream>/x86_64/<filename>` are resolved through
`publication` to the store object. The same `any` artifact can be live in
both repos with one copy on disk.

Client `pacman.conf`, stable machine:

```
[repoman]
SigLevel = Required DatabaseRequired
Server = https://nas/repoman/repos/manjaro-stable/$arch
```

Upstream Manjaro repos on the same machine add
`CacheServer = https://nas/repoman/cache` to their stanzas.

### 9.3 Build-once rule for `any`

An `arch=any` package is built once, on the lowest-sort member stream, and
published to all member streams if none of its depends is in the configured
versioned-interpreter list (`python`, `perl`, `ruby`, `lua`, and others whose
install paths encode a version). Otherwise it is built per stream like
everything else. The policy field can override either way.

## 10. The gate

### 10.1 Provenance class

Assigned at onboarding from the baseline PKGBUILD, confirmed by the operator,
editable later:

- `upstream-binary` — `-bin` from the upstream project's own release host.
- `upstream-source` — tarball or tag from the upstream project's own forge.
- `vcs` — builds from a VCS head.
- `repackaged` — converts deb, rpm, AppImage, npm, pip, crate, or similar.
- `authored` — the AUR repo carries real content: patches, scripts, units.

The class defines the expected shape of a routine update.

### 10.2 Diff classification

Baseline SRCINFO versus head SRCINFO, field by field, plus the file list of
the AUR repo. The `inspect` job's regenerated SRCINFO must match the committed
one, or the candidate is structural.

| Class | Meaning | Auto-approve |
|-------|---------|--------------|
| routine | pkgver, pkgrel, checksums, and source URLs that differ only by version substitution against the baseline template | yes, after quarantine |
| metadata | pkgdesc, license, url, optdepends, official-repo depends | yes, after quarantine, if policy allows |
| structural | new or changed source entries, source host or owner change, `validpgpkeys`, install script, any function body, new files, new AUR dependencies, arch changes, version going backwards, SRCINFO mismatch | no |

### 10.3 Floor risk

Computed deterministically from:

- Diff class.
- Maintainer or co-maintainer changed since baseline → structural.
- Danger lint over PKGBUILD and install scripts: network tools inside
  functions, `eval`, base64 or hex decoding, writes outside `pkgdir` and
  `srcdir`, `sudo`, URL shorteners, IP-literal or plain-http URLs, Unicode
  bidi or zero-width characters, new pacman hooks, systemd units, sudoers,
  polkit rules.
- Corroboration for routine updates via forge adapters (GitHub, GitLab,
  Codeberg in M2; others as needed): the version exists upstream as a tag or
  release, and the release owner is unchanged.
- Post-build inputs (step 11): artifact diff against the previous
  publication showing new privileged surface or setuid bits; writes outside
  expected paths; network attempts (M2).

### 10.4 Quarantine

A routine or metadata candidate auto-approves only when
`now − first_seen_last_modified` exceeds the configured age, default 3 days,
and no negative signal exists for its commit. `first_seen_last_modified` is
the AUR server's timestamp as first observed by repoman, never the git commit
date. The operator can expedite any candidate. VCS and dependency-rebuild
candidates have no quarantine.

### 10.5 Signals

- aur-security lookup API by pkgbase and commit list.
- AUR state: package deleted, orphaned, out-of-date flag, maintainer change.
- Future: aur-general or aur-requests mailbox ingestion.

Signals are stored with the commit they concern and shown on the candidate.
They raise risk; they never approve.

### 10.6 LLM reviewer

Direct `POST /v1/messages` with:

- Model `claude-opus-5-5` for structural candidates, `claude-sonnet-5-5` for
  bulk routine triage when enabled.
- System prompt describing the task and the output contract. The diff, both
  PKGBUILDs, install scripts, and the heuristic findings go in the user turn,
  framed explicitly as untrusted data.
- Structured output constrained to a schema: `risk` enum, `summary`,
  `findings[]` with `kind`, `evidence`, `severity`, and `questions_for_human[]`.
- No tools. Server-side fallback enabled so a classifier refusal degrades to a
  recorded "no verdict" rather than a failure.
- Prompt, response, model, token usage, and cost stored on the candidate.

Rule: `final_risk = max(floor_risk, llm_risk)`. The reviewer's text is shown
in the queue so the operator reviews a classified, explained change instead
of a raw diff.

### 10.7 Candidate lifecycle

```
pending ──approve──► approved ──plan──► building ──ok──► built ──clean──► published
   │                    ▲                  │                 │
   ├──reject──► rejected (until head moves) │                 └──flags──► publish_review ──► published | rejected
   └──defer───► deferred                    └──fail x2──► failed (stays approved; re-planned next run)
```

The baseline advances only on `published`. A superseding head commit closes
any open candidate for the older commit; the new candidate's diff runs from
the published baseline, so the operator sees the cumulative change.

### 10.8 Onboarding

A one-time working session produces the wanted list from the two machines'
foreign package lists, the v1 list, and the official repos. Each package gets
a provenance class, stream membership, and a baseline at its current AUR
commit through `onboarding` candidates in the first run. No persistent audit
metadata. Part of M1.

## 11. Publishing and signing

- Dedicated signing key, no passphrase, in the coordinator's GPG home on the
  NAS SSD. Clients import and locally sign it once.
- Each artifact gets a detached signature stored beside it in the content
  store. `repo-add --sign --verify` writes each stream's database from the
  store paths. New database and signature are written to temp names and
  renamed into place after every referenced artifact is live in
  `publication`, so a client never sees a database referencing a missing
  file.
- Publishing a new version retires the previous publication of the same
  pkgname in that repo. Retention keeps `N` retired versions per pkgname per
  repo, default 2, still served for rollback until pruned. A store object is
  deleted when it has no live or retained publication.
- Staging repos: one per run and stream, unsigned, served only to workers
  from the store, deleted at run end.
- Build scratch on workers is deleted on success and kept at most `M` days on
  failure.

## 12. Cache role

HTTP endpoint `/cache/{filename}` for upstream repos only, consumed by workers
and clients via `CacheServer`.

- **Hit** — serve the file with range support.
- **Miss** — resolve filename to repo and stream via the sync-DB index; if
  unknown, try each stream's mirror in order. Download to a temp file, verify
  size and checksum against the sync DB entry when known, rename into place,
  serve. Concurrent requests for one filename share a per-file lock.
  In-flight requests that cannot wait receive 503 and pacman falls back to
  `Server`.
- **Referenced set** — recomputed after every stream refresh: every filename
  listed in any stream's current databases.
- **Eviction** — a file leaving the referenced set enters a grace period,
  default 30 days. After grace it is deleted. A total size cap evicts the
  least recently served evictable files early. Sizes and policy are shown in
  the UI.
- **Serving is never gated by the referenced set.** A client ahead of the
  coordinator's refresh gets a miss, not a refusal.

The goal is that each upstream file crosses the internet once for all
workers and clients; pacman's fallback to `Server` means this is a goal, not
a guarantee.

## 13. Security posture summary

- Builds run with no network in the build phase, as an unprivileged uid, in a
  throwaway overlay, under a memory limit and disk quota.
- Nothing publishes without passing the gate; structural changes always wait
  for a human; post-build evidence can pull an approved build back for review.
- The LLM sees untrusted content as data, has no tools, and cannot lower risk.
- Quarantine uses a server-side clock the pusher cannot forge.
- Signing happens only on the coordinator; workers never hold the key.
- Workers authenticate with per-worker tokens; artifacts are content-addressed
  and their checksums recorded before signing.
- Every decision, signal, prompt, and response is logged for audit.

## 14. Testing strategy

- **Unit** — SRCINFO parser, sync-DB parser, version comparison against the
  pacman test vectors, diff classifier, danger lint, ELF NEEDED scanner,
  eviction policy, DAG planner including dependency discovery loops,
  candidate state machine.
- **Classifier golden tests** — fixtures modelled on the 2026 AUR campaigns:
  adopted orphan plus install-script payload, npm-based payload fetched in
  `prepare()` or `package()`, typosquat with foreign source host, backdated
  commit inside quarantine, and benign version bumps of each provenance
  class. Every fixture asserts the class and floor risk.
- **Integration** — a fake AUR: a local git server with package repos and a
  stub RPC endpoint; a local stream: a minimal pacman repo with a few real
  packages; a local mirror for the cache. Full update runs execute against
  these with no internet, including a run that discovers a transitive AUR
  dependency and loops.
- **Executor** — requires root, namespaces, and cgroup v2; runs in a
  privileged test container or on the desktop via
  `cargo test --features exec-tests`. Covers provisioning, network-off
  enforcement including a `prepare()` that tries to fetch, upper-dir diff,
  memory-limit kill and retry, quota, timeouts.
- **Cache** — concurrent miss coalescing, range requests, verification
  failure handling, eviction with a mocked clock.
- **Publisher** — atomic database swap under concurrent reads, `any`
  artifact live in two repos with one store object, retention and retire
  bookkeeping.
- **LLM reviewer** — recorded API responses for deterministic tests; a live
  smoke test behind a feature flag.

## 15. Milestones

Each milestone ends with a commit, a sub-agent review focused on the named
risk, and a working system the operator can use.

### M0 — Skeleton (1 session)
Crate layout, config format, SQLite schema and migrations, event log, CLI
scaffold, coordinator container running on the NAS, worker on the desktop
connected over WebSocket and executing a no-op job with timestamped logs and
stage events. Exit: `repoman workers` lists the desktop; a hello job round-trips
and survives a worker reconnect.
Review focus: protocol reconnection and idempotent job states.

### M1 — Build and publish, CLI only (3 sessions)

Status 2026-10-31: implemented and running against the NAS coordinator with
a user-namespace worker on the desktop. Verified on real packages: release
(uhubctl), VCS with probe-driven rebuilds (joycond-git), arch=any build-once
(pico-sdk, picotool), dependency discovery (picotool → pico-sdk), signed
publishing to both stream repos, idle runs planning nothing. Remaining for
the exit criterion: onboarding the real wanted list, the root system unit on
the desktop, and switching clients.
Streams with sync-DB parsing, base image builder and distribution, executor
with all phases, cgroup limits, recorded peaks, network-off build, per-stream
staging repos, planner with AUR dependency DAG and discovery loop, PGP key
cache, content store, signing, atomic publish, retention, `any` build-once
rule. Gate in manual mode: every candidate needs CLI approval; onboarding
candidates set class and membership. Onboarding session for the wanted list.
Exit: every package on the wanted list is built for its member streams
through v2, clients are switched to the new repos with signature enforcement,
v1 is stopped.
Review focus: executor mount, namespace, and cgroup edge cases; publish
atomicity; dependency discovery loop termination.

### M2 — Gate and web UI (2–3 sessions)
Diff classifier, danger lint, provenance classes, quarantine, inspect jobs,
forge adapters for GitHub, GitLab, Codeberg, artifact diff, nftables
network-attempt counting, API reviewer with structured output, candidate
lifecycle UI, run and job views, log viewer, package pages with history.
Exit: a full update run is driven from the browser; routine updates
auto-approve after quarantine; golden tests pass.
Review focus: classifier false negatives against the fixture set; lifecycle
edge cases around failed and superseded candidates.

### M3 — Cache role (1–2 sessions)
Pull-through cache with verification, locking, ranges, referenced-set
eviction, size cap, UI page. Workers and clients switch to `CacheServer` for
upstream repos.
Exit: a fresh base image build pulls every package from the cache after the
first run; eviction removes stale files on schedule.
Review focus: concurrency and partial-download handling.

### M4 — Scale and signals (1–2 sessions)
Laptop as a second worker, admission by predicted footprint, aur-security
signal integration, `soname_rebuild` candidates when a stream's sync DB
changes a soname that a published artifact needs, VCS probe scheduling
policy.
Exit: a soname bump in a stream produces rebuild candidates on the next run
without operator input; two workers share a run without duplicate jobs.
Review focus: scheduler fairness, duplicate-job suppression, footprint
prediction on first-seen packages.

## 16. Deferred and open

- composefs-based provisioning behind the `Provisioner` trait, if measured
  dep-install time ever dominates.
- `x86_64_v3` build profile as a second dimension next to arch.
- Mailing-list ingestion for aur-general and aur-requests.
- Running a worker on the NAS itself.
- Reproducible-build verification from `.BUILDINFO`.
- A second LLM provider behind the `Reviewer` trait.
- Cross-stream binary sharing, if a sound compatibility oracle ever exists.

## 17. Operational notes

- Coordinator deployment: stack at `/containers/repoman` on the NAS per that
  host's conventions, see `deploy/compose.yaml`. Container `repoman` on the
  `docker1` macvlan, reachable as `repoman.b.intern.gecko.network:7800`.
  SQLite at `/srv/db/repoman/coordinator` on the SSD; GPG home joins it in
  M1. Store, repos, images, and cache go under `/media/data/repoman` on the
  HDD array from M1. UI and repos exposed through the existing reverse proxy,
  which forwards to the coordinator rather than serving static files. Source
  is rsync'd to the stack's `src/` and the image is built on the host.
- Worker deployment: M0 runs a user-level systemd unit on the desktop with the
  binary in `~/.local/bin`, since the no-op executor needs no privileges. M1
  moves it to a system service with `ProtectSystem=strict` and explicit read-write paths for scratch, images,
  and `SRCDEST`; `CAP_SYS_ADMIN` retained for mounts and namespaces;
  delegated cgroup subtree.
- Configuration: one TOML per role. Secrets (worker tokens, API key) via
  environment or a secrets file, never in the database.
- Backups: SQLite and GPG home nightly on the NAS's existing backup path.
  Store, repos, images, and cache are reproducible and excluded.
