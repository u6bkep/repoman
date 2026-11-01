// repoman operator UI. Plain JS over the JSON API; hash routing; views
// that show live state re-fetch every few seconds while visible. A refresh
// swaps the page's HTML but keeps scroll positions and open <details>, and
// the job log appends only what is new.

const $main = document.getElementById("main");
const $status = document.getElementById("status");
let refreshTimer = null;

// ---- helpers -----------------------------------------------------------------

async function api(path, opts = {}) {
  const r = await fetch(path, { headers: { "content-type": "application/json" }, ...opts });
  if (!r.ok) {
    const text = await r.text();
    throw new Error(`${opts.method || "GET"} ${path}: ${r.status} ${text}`);
  }
  if (r.status === 204) return null;
  return r.json();
}
const post = (path, body) => api(path, { method: "POST", body: JSON.stringify(body || {}) });
const del = (path) => api(path, { method: "DELETE" });

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}
function short(id, n = 10) { return id ? String(id).slice(0, n) : "-"; }
function dash(v) { return v === null || v === undefined || v === "" ? "-" : esc(v); }
function pill(v, label) { return v ? `<span class="pill ${esc(v)}">${esc(label ?? v)}</span>` : "-"; }
function absolute(ts) { return ts ? new Date(ts).toLocaleString() : ""; }
function relative(ts) {
  if (!ts) return "-";
  const s = (Date.now() - Date.parse(ts)) / 1000;
  const a = Math.abs(s);
  const n = a < 60 ? `${Math.floor(a)}s` : a < 3600 ? `${Math.floor(a / 60)}m` : a < 86400 ? `${Math.floor(a / 3600)}h` : `${Math.floor(a / 86400)}d`;
  return s >= 0 ? `${n} ago` : `in ${n}`;
}
// Relative time with the absolute one on hover; use everywhere a time shows.
function ago(ts) { return ts ? `<span title="${esc(absolute(ts))}">${relative(ts)}</span>` : "-"; }
function agoEpoch(secs) { return secs ? ago(new Date(secs * 1000).toISOString()) : "-"; }
function dur(a, b) {
  if (!a) return "-";
  const s = Math.max(0, (Date.parse(b || new Date().toISOString()) - Date.parse(a)) / 1000);
  return s < 60 ? `${Math.round(s)}s` : s < 3600 ? `${Math.floor(s / 60)}m ${Math.round(s % 60)}s` : `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}
function bytes(n) {
  if (n === null || n === undefined) return "-";
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0; let v = Number(n);
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v < 10 && i ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
}
function toast(msg, isError = false) {
  let t = document.querySelector(".toast");
  if (!t) { t = document.createElement("div"); t.className = "toast"; document.body.appendChild(t); }
  t.textContent = msg; t.style.background = isError ? "var(--high)" : ""; t.classList.add("show");
  clearTimeout(t._timer); t._timer = setTimeout(() => t.classList.remove("show"), isError ? 6000 : 2500);
}
function table(headers, rows, opts = {}) {
  const ths = headers.map((h) => `<th>${esc(h)}</th>`).join("");
  const trs = rows.map((r) => {
    const href = opts.href ? opts.href(r) : null;
    const tds = opts.cells(r).map((c) => `<td>${c}</td>`).join("");
    return href ? `<tr class="click" data-href="${esc(href)}">${tds}</tr>` : `<tr>${tds}</tr>`;
  }).join("");
  return `<table class="${opts.cls || ""}"><thead><tr>${ths}</tr></thead><tbody>${trs || `<tr><td colspan="${headers.length}" class="muted">${esc(opts.empty || "nothing here")}</td></tr>`}</tbody></table>`;
}
function kv(pairs) {
  return `<dl class="kv">${pairs.filter((p) => p && p[1] !== undefined && p[1] !== null && p[1] !== "").map(([k, v]) => `<dt>${esc(k)}</dt><dd>${v}</dd>`).join("")}</dl>`;
}
function renderDiff(text) {
  return text.split("\n").map((l) => {
    if (l.startsWith("+++") || l.startsWith("---")) return `<span class="meta">${esc(l)}</span>`;
    if (l.startsWith("@@")) return `<span class="hunk">${esc(l)}</span>`;
    if (l.startsWith("+")) return `<span class="add">${esc(l)}</span>`;
    if (l.startsWith("-")) return `<span class="del">${esc(l)}</span>`;
    return `${esc(l)}\n`;
  }).join("").replace(/<\/span>(?!\n)/g, "</span>");
}
function listOrNone(items, max = 8) {
  if (!items || !items.length) return '<span class="muted">none</span>';
  const shown = items.slice(0, max).map(esc).join(", ");
  return items.length > max ? `${shown} <span class="muted">+${items.length - max} more</span>` : shown;
}

// ---- rendering ---------------------------------------------------------------------
// Views produce the whole page as a string. Applying it by morphing the
// live DOM, instead of replacing it, keeps every node that is still
// there, and with it everything the browser holds on the reader's behalf:
// scroll offsets, open <details>, focus, selection, half-typed input.
// Nodes match by id, then data-key, then tag at the same position.

// Attributes the browser or the reader owns; never written by a morph.
const OWNED_ATTRS = new Set(["open", "value", "checked", "selected"]);

function morphAttrs(from, to) {
  for (const a of [...from.attributes]) {
    if (!OWNED_ATTRS.has(a.name) && !to.hasAttribute(a.name)) from.removeAttribute(a.name);
  }
  for (const a of to.attributes) {
    if (!OWNED_ATTRS.has(a.name) && from.getAttribute(a.name) !== a.value) from.setAttribute(a.name, a.value);
  }
}
function keyOf(n) { return n.nodeType === 1 ? (n.id || n.dataset?.key || null) : null; }
function same(a, b) { return a.nodeType === b.nodeType && (a.nodeType !== 1 || a.tagName === b.tagName); }
function morphNode(from, to) {
  if (!same(from, to)) { from.replaceWith(to.cloneNode(true)); return; }
  if (from.nodeType === 3 || from.nodeType === 8) { if (from.data !== to.data) from.data = to.data; return; }
  if (from === document.activeElement && /^(INPUT|TEXTAREA|SELECT)$/.test(from.tagName)) return;
  morphAttrs(from, to);
  // An element the view fills by other means (the log) keeps its children.
  if (from.hasAttribute("data-morph-skip")) return;
  morphChildren(from, to);
}
function morphChildren(from, to) {
  const olds = [...from.childNodes];
  const used = new Set();
  let placed = null; // last node settled into position; the next goes after it
  let cursor = 0;    // olds index after the last positional match
  for (const want of to.childNodes) {
    const key = keyOf(want);
    let idx = -1;
    if (key) {
      idx = olds.findIndex((o, i) => !used.has(i) && keyOf(o) === key);
    } else {
      // The next unconsumed old node, if it is unkeyed and of the same kind.
      let i = cursor;
      while (i < olds.length && used.has(i)) i++;
      if (i < olds.length && !keyOf(olds[i]) && same(olds[i], want)) idx = i;
    }
    const slot = placed ? placed.nextSibling : from.firstChild;
    let node;
    if (idx >= 0) {
      node = olds[idx];
      used.add(idx);
      cursor = idx + 1;
      // Moving a node that is already in place would drop its focus and
      // scroll state, so only move when something else occupies the slot.
      if (node !== slot) from.insertBefore(node, slot);
      morphNode(node, want);
    } else {
      node = want.cloneNode(true);
      from.insertBefore(node, slot);
    }
    placed = node;
  }
  for (let i = 0; i < olds.length; i++) if (!used.has(i) && olds[i].parentNode === from) from.removeChild(olds[i]);
}
let lastHtml = null;
function swap(html) {
  if (html === lastHtml) return;
  const tpl = document.createElement("template");
  tpl.innerHTML = html;
  morphChildren($main, tpl.content);
  lastHtml = html;
}

// One line describing what an active run is doing right now.
function progressLine(run) {
  const p = run?.progress || {};
  const since = p.at ? ` <span class="muted">(${ago(p.at)})</span>` : "";
  switch (p.phase) {
    case "streams": case "poll": case "images": return `${esc(p.detail || p.phase)}${since}`;
    case "candidates": return `${esc(p.detail || "creating candidates")}${since}`;
    case "gate": return `gate: deciding routine candidates${since}`;
    case "probes": {
      const left = (p.total ?? 0) - (p.done ?? 0);
      return `probing VCS upstreams: ${p.done ?? 0}/${p.total ?? 0}${left ? `, ${left} still answering` : ""}${p.last ? ` (last: ${esc(p.last)} on ${esc(p.worker || "?")})` : ""}${since}`;
    }
    case "plan": return `planning builds${since}`;
    case "build": {
      const running = (p.running || []).map((j) => `<a href="#/jobs/${esc(j.job)}">${esc(j.pkgbase)}</a> on ${esc(j.worker || "?")} for ${dur(j.since)}`).join(", ");
      return `building: ${p.done ?? 0} done, ${p.failed ?? 0} failed, ${p.queued ?? 0} queued of ${p.total ?? 0}${running ? ` · running ${running}` : ""}${since}`;
    }
    case "publish": return `publishing${since}`;
    case "done": return `finishing${since}`;
    default: return run?.step ? `${esc(run.step)}${since}` : "";
  }
}

function versionChange(r) {
  if (r.version_from && r.version_to) return `${esc(r.version_from)} → ${esc(r.version_to)}`;
  return r.version_to ? esc(r.version_to) : "";
}
function candidateSummary(c) {
  const r = c.review || {};
  const parts = [];
  const reasons = (r.gate?.reasons || []).filter((x) => x.risk !== "low").map((x) => x.kind);
  if (reasons.length) parts.push([...new Set(reasons)].join(" "));
  const n = (r.gate?.findings || []).filter((f) => f.severity !== "low").length;
  if (n) parts.push(`${n} lint finding${n === 1 ? "" : "s"}`);
  if (r.upstream_rev) parts.push(`upstream ${short(r.upstream_rev, 12)}`);
  if (c.note) parts.push(c.note);
  if (r.note && !parts.length) parts.push(r.note);
  return parts.join("; ");
}

// ---- decision logic mirrored from the engine's auto_approve ----------------------
// Used to tell the reader what will happen on its own, and why something
// needs a human. Config comes from /api/config; package policy from the
// package rows; blocking signals from /api/signals.

let ctx = { config: null, policies: {}, blocking: [] };
async function loadContext() {
  const [config, pkgs, blocking] = await Promise.all([api("/api/config"), api("/api/packages"), api("/api/signals")]);
  const policies = {};
  for (const p of pkgs) policies[p.pkgbase] = p.policy || {};
  ctx = { config, policies, blocking, packages: pkgs };
  return ctx;
}
function quarantineDays(pkgbase) {
  const pol = ctx.policies[pkgbase] || {};
  return pol.quarantine_days ?? ctx.config?.quarantine_days ?? 3;
}
// Why the gate will not approve this on its own, as a list of short reasons;
// empty means it will, once quarantine ends.
function needsHumanBecause(c) {
  const r = c.review || {};
  const out = [];
  if (c.kind === "onboarding") out.push("first review of a new package");
  if (c.kind !== "update" && c.kind !== "onboarding") out.push(`${c.kind} candidate`);
  if (c.state === "publish_review") out.push("built artifacts raised flags");
  if (c.state === "failed") out.push("build failed");
  if (r.gate_error) out.push("gate could not assess it");
  const cls = r.gate?.diff_class;
  if (cls === "structural") out.push(`structural: ${[...new Set((r.gate.reasons || []).filter((x) => x.class === "structural").map((x) => x.kind))].join(", ") || "see reasons"}`);
  if (cls === "metadata" && !(ctx.config?.auto_approve_metadata && (ctx.policies[c.pkgbase]?.auto_metadata ?? true))) out.push("metadata change; auto-approval of metadata is off");
  if (c.floor_risk && c.floor_risk !== "low") out.push(`lint floor is ${c.floor_risk}`);
  if (c.llm_risk && c.llm_risk !== "low") out.push(`reviewer says ${c.llm_risk}`);
  const sig = ctx.blocking.filter((s) => s.pkgbase === c.pkgbase);
  if (sig.length) out.push(`${sig.length} blocking signal${sig.length > 1 ? "s" : ""}: ${[...new Set(sig.map(sigLabel))].join(", ")}`);
  if (!ctx.config?.auto_approve) out.push("auto-approval is disabled");
  return out;
}
function quarantineEnds(c) {
  if (!c.first_seen_last_modified) return null;
  return new Date((c.first_seen_last_modified + quarantineDays(c.pkgbase) * 86400) * 1000).toISOString();
}
// "gate approves in 2d 3h" / "gate approves at next run" / list of blockers.
function decisionCell(c) {
  const why = needsHumanBecause(c);
  if (why.length) return `<span class="pill high">you</span> <span class="muted">${esc(why.join("; "))}</span>`;
  const ends = quarantineEnds(c);
  if (!ends) return `<span class="pill high">you</span> <span class="muted">no AUR timestamp</span>`;
  return Date.parse(ends) <= Date.now()
    ? `<span class="pill low">gate</span> <span class="muted">approves at the next run</span>`
    : `<span class="pill low">gate</span> <span class="muted" title="${esc(absolute(ends))}">approves ${relative(ends)} (quarantine ${quarantineDays(c.pkgbase)}d)</span>`;
}

// ---- signals -----------------------------------------------------------------------

function sigActive(s) { return !s.cleared_at && !(s.expires_at && s.expires_at <= new Date().toISOString()); }
function sigLabel(s) {
  const p = s.payload || {};
  if (s.source === "aur-security") return p.verdict || "";
  if (s.source === "aur-requests") return p.kind === "request_outcome" ? `${p.request} request #${p.prq} ${p.outcome}` : `${p.request} request #${p.prq}`;
  if (s.source === "aur-general") return `mentioned in “${p.subject || ""}”`;
  return { adopted: `adopted by ${p.by}`, deleted: "deleted from the AUR", resubmitted: "deleted and resubmitted under the same name", history_rewritten: "git history replaced" }[p.kind] || p.kind || "";
}
function sigDetail(s) {
  const p = s.payload || {};
  if (s.source === "aur-security") return p.explanation || "";
  if (s.source === "aur-requests") return p.kind === "request_outcome" ? `by ${p.who || "?"}` : (p.body || "").split("\n").filter(Boolean).slice(1).join(" ").slice(0, 300);
  if (s.source === "aur-general") return `${p.who ? `${p.who}: ` : ""}${p.excerpt || ""}`;
  switch (p.kind) {
    case "adopted": return `orphaned since ${p.orphaned_since ? relative(p.orphaned_since) : "before we watched"}; nothing auto-approves for ${p.quarantine_days} days`;
    case "deleted": return `missing since ${relative(p.missing_since)}; git repository ${p.git_gone ? "gone too" : "still answers"}; last ${p.last_version || "?"} by ${p.last_maintainer || "nobody"}`;
    case "resubmitted": return `base id ${p.old_id} → ${p.new_id}, first submitted ${relative(new Date(p.old_first_submitted * 1000).toISOString())} → ${relative(new Date(p.new_first_submitted * 1000).toISOString())}${p.missing_since ? `, missing since ${relative(p.missing_since)}` : ""}`;
    case "history_rewritten": return `baseline ${short(p.baseline)} is not an ancestor of ${short(p.head)}`;
    default: return JSON.stringify(p);
  }
}
function sigState(s) {
  if (s.cleared_at) return `<span class="pill">cleared by ${esc(s.cleared_by)} ${ago(s.cleared_at)}</span>`;
  if (s.expires_at && s.expires_at <= new Date().toISOString()) return `<span class="pill">expired ${ago(s.expires_at)}</span>`;
  if (s.expires_at) return `<span class="muted">expires ${ago(s.expires_at)}</span>`;
  return "";
}
function signalsList(list, { withPackage = false } = {}) {
  if (!list || !list.length) return "";
  return `<ul class="findings">${list.map((s) => {
    const active = sigActive(s);
    return `<li class="${active ? "" : "muted"}">${pill(s.severity)} ${withPackage ? `<a href="#/packages/${esc(s.pkgbase)}"><b>${esc(s.pkgbase)}</b></a> ` : ""}<b>${esc(s.source)}</b> ${esc(sigLabel(s))} ${s.aur_commit ? `<span class="mono">${short(s.aur_commit)}</span>` : '<span class="muted">whole package</span>'} <span class="muted">${esc(sigDetail(s))}</span>${s.payload?.link ? ` <a href="${esc(s.payload.link)}" target="_blank" rel="noopener">message</a>` : ""} ${sigState(s)} ${ago(s.observed_at)}${active && s.severity !== "info" ? ` <button data-clear-signal="${esc(s.id)}">Clear</button>` : ""}</li>`;
  }).join("")}</ul>`;
}
function wireSignalClears() {
  document.querySelectorAll("[data-clear-signal]").forEach((b) => b.onclick = async () => {
    b.disabled = true;
    try { await post(`/api/signals/${b.dataset.clearSignal}/clear`, { by: "ui" }); toast("signal cleared"); route(); }
    catch (e) { toast(e.message, true); b.disabled = false; }
  });
}
// What the AUR currently says about a package, as pills.
function aurStatePills(p) {
  const s = p.snapshot;
  if (!s || p.source_url) return "";
  const out = [];
  if (s.missing_since) out.push(`<span class="pill high" title="missing since ${esc(absolute(s.missing_since))}">gone from AUR ${relative(s.missing_since)}</span>`);
  if (!s.maintainer && !s.missing_since) out.push(`<span class="pill medium" title="orphaned since ${esc(absolute(s.orphaned_since) || "?")}">orphan${s.orphaned_since ? ` ${relative(s.orphaned_since)}` : ""}</span>`);
  if (s.out_of_date) out.push(`<span class="pill medium" title="flagged ${esc(new Date(s.out_of_date * 1000).toLocaleString())}">out of date</span>`);
  const blocking = (p.signals || []).filter((x) => sigActive(x) && x.severity !== "info");
  if (blocking.length) out.push(`<span class="pill high">${blocking.length} blocking signal${blocking.length > 1 ? "s" : ""}</span>`);
  return out.join(" ");
}
// Per-stream served version against the AUR's, with a "behind" marker.
function liveCell(p) {
  if (!p.live?.length) return '<span class="muted">nothing served</span>';
  const aur = p.snapshot?.version;
  const byRepo = {};
  for (const v of p.live) (byRepo[v.repo] ||= []).push(v);
  return Object.entries(byRepo).map(([repo, vs]) => {
    const ver = vs[0].pkgver;
    const behind = p.kind !== "vcs" && aur && ver !== aur;
    return `<span class="nowrap"><span class="muted">${esc(repo.replace("manjaro-", ""))}</span> <span title="published ${esc(absolute(vs[0].published_at))}">${esc(ver)}</span>${behind ? ` <span class="pill medium" title="AUR has ${esc(aur)}">behind</span>` : ""}</span>`;
  }).join("<br>");
}

// ---- routing -----------------------------------------------------------------

const routes = [
  [/^#\/queue$/, viewQueue],
  [/^#\/candidates\/([^/]+)$/, viewCandidate],
  [/^#\/runs$/, viewRuns],
  [/^#\/runs\/([^/]+)$/, viewRun],
  [/^#\/jobs\/([^/]+)$/, viewJob],
  [/^#\/packages$/, viewPackages],
  [/^#\/packages\/([^/]+)$/, viewPackage],
  [/^#\/system$/, viewSystem],
];

async function route() {
  clearInterval(refreshTimer); refreshTimer = null;
  const hash = location.hash || "#/queue";
  for (const [re, view] of routes) {
    const m = hash.match(re);
    if (m) {
      document.querySelectorAll("nav a").forEach((a) => a.classList.toggle("active", hash.startsWith(`#/${a.dataset.nav}`) || (a.dataset.nav === "queue" && hash.startsWith("#/candidates")) || (a.dataset.nav === "runs" && hash.startsWith("#/jobs"))));
      $main.dataset.view = "";
      lastHtml = null;
      const render = async () => {
        try {
          await view(...m.slice(1).map(decodeURIComponent));
        } catch (e) {
          if (location.hash === hash) swap(`<div class="panel"><b>Error:</b> ${esc(e.message)}</div>`);
        }
      };
      await render();
      if (view.refresh) refreshTimer = setInterval(() => { if (!document.hidden && !document.activeElement.matches("input,textarea,select")) render(); }, view.refresh);
      return;
    }
  }
  location.hash = "#/queue";
}
window.addEventListener("hashchange", route);
document.addEventListener("click", (e) => {
  const tr = e.target.closest("tr.click");
  if (tr && !e.target.closest("a,button,details,summary")) location.hash = tr.dataset.href;
});

async function header() {
  try {
    const [h, pending, workers, runs] = await Promise.all([api("/api/health"), api("/api/candidates"), api("/api/workers"), api("/api/runs")]);
    document.getElementById("queue-count").textContent = pending.length || "";
    const online = workers.filter((w) => w.connected).length;
    const active = runs.find((r) => r.state === "running");
    let phase = "idle";
    if (active) {
      const p = active.progress || {};
      phase = p.phase === "build" ? `building ${p.done ?? 0}/${p.total ?? 0}` : p.phase === "probes" ? `probing ${p.done ?? 0}/${p.total ?? 0}` : p.detail || p.phase || active.step || "running";
    } else if (runs.some((r) => r.state === "needs_input")) {
      phase = "waiting for decisions";
    }
    const busy = workers.reduce((n, w) => n + (w.running?.length || 0), 0);
    $status.textContent = `${phase} · ${online}/${workers.length} workers${busy ? `, ${busy} job${busy > 1 ? "s" : ""} running` : ""}`;
  } catch (e) { $status.textContent = "coordinator unreachable"; }
}
setInterval(header, 5000);

// ---- queue -------------------------------------------------------------------

async function viewQueue() {
  const [pending, deferred, runs] = await Promise.all([
    api("/api/candidates"), api("/api/candidates?state=deferred"), api("/api/runs"), loadContext(),
  ]);
  const active = runs.find((r) => r.state === "running" || r.state === "needs_input");
  const activeRun = active ? await api(`/api/runs/${active.id}`) : null;
  const held = pending.filter((c) => c.state === "publish_review");
  const rest = pending.filter((c) => c.state !== "publish_review");
  const you = rest.filter((c) => needsHumanBecause(c).length);
  const clock = rest.filter((c) => !needsHumanBecause(c).length);
  const headers = ["package", "change", "class", "diff", "floor", "llm", "on AUR", "decides", "notes"];
  const cells = (c) => [
    `<a href="#/packages/${esc(c.pkgbase)}">${esc(c.pkgbase)}</a> <span class="muted">${esc(c.kind)}</span>`,
    versionChange(c.review || {}) || dash(c.review?.upstream_rev && short(c.review.upstream_rev, 12)),
    dash(c.class), pill(c.review?.gate?.diff_class), pill(c.floor_risk), pill(c.llm_risk),
    agoEpoch(c.first_seen_last_modified),
    decisionCell(c),
    `<span class="ellipsis">${esc(candidateSummary(c))}</span>`,
  ];
  const opts = { cells, href: (c) => `#/candidates/${c.id}` };
  swap(`
    <div class="row between">
      <h1>Queue</h1>
      <div class="row">
        ${active ? `<a href="#/runs/${active.id}">run ${short(active.id)} ${pill(active.state)}</a>` : `<span class="muted">no run active</span>`}
        <button class="primary" id="start-run">${active?.state === "needs_input" ? "Resume run" : "Start run"}</button>
      </div>
    </div>
    ${activeRun && activeRun.state === "running" ? `<div class="panel">run <a href="#/runs/${activeRun.id}" class="mono">${short(activeRun.id)}</a> · ${progressLine(activeRun)}</div>` : ""}
    <h2>Needs you (${you.length + held.length})</h2>
    ${held.length ? `<p class="muted">Built, but the artifacts differ from what the stream serves in a way that matters; approve to publish, reject to drop.</p>${table(headers, held, { ...opts, empty: "" })}` : ""}
    ${table(headers, you, { ...opts, empty: held.length ? "" : "nothing waits on a decision" })}
    <h2>Waiting on the clock (${clock.length})</h2>
    <p class="muted">Routine changes the gate will approve once they have been visible on the AUR for the quarantine period. Approve to skip the wait, reject to drop; otherwise leave them.</p>
    ${table(headers, clock, { ...opts, empty: "nothing in quarantine" })}
    ${deferred.length ? `<h2>Deferred (${deferred.length})</h2>${table(headers, deferred, opts)}` : ""}
  `);
  document.getElementById("start-run").onclick = async (e) => {
    e.target.disabled = true;
    try { const r = await post("/api/runs"); toast(`run ${short(r.id)} ${r.resumed ? "resumed" : "started"}`); location.hash = `#/runs/${r.id}`; }
    catch (err) { toast(err.message, true); e.target.disabled = false; }
  };
}
viewQueue.refresh = 5000;

// ---- candidate -------------------------------------------------------------------

function artifactsTable(jobs) {
  const rows = [];
  for (const j of jobs) for (const a of j.result?.artifacts || []) rows.push({ j, a });
  if (!rows.length) return "";
  return table(["stream", "file", "version", "size", "net attempts", "outside writes", "setuid", "job"], rows, {
    cells: ({ j, a }) => [dash(j.stream), `<span class="mono">${esc(a.filename)}</span>`, esc(a.pkgver), bytes(a.size),
      j.result.net_attempts ? `<span class="pill ${j.job?.allow_network ? "" : "medium"}">${j.result.net_attempts}</span>` : "0",
      (j.result.outside_writes || []).length ? `<span class="pill medium">${j.result.outside_writes.length}</span>` : "0",
      (a.setuid_paths || []).length ? `<span class="pill high">${esc(a.setuid_paths.join(" "))}</span>` : "none",
      `<a href="#/jobs/${esc(j.id)}" class="mono">${short(j.id)}</a>`],
  });
}

function changesTable(changes) {
  if (!changes?.length) return "";
  const diffLists = (before, after) => {
    const gone = before.filter((x) => !after.includes(x)).map((x) => `<span class="del">−${esc(x)}</span>`);
    const added = after.filter((x) => !before.includes(x)).map((x) => `<span class="add">+${esc(x)}</span>`);
    return [...gone, ...added].join(" ") || '<span class="muted">reordered</span>';
  };
  return table(["section", "field", "change"], changes, {
    cells: (c) => [esc(c.section), `<code>${esc(c.key)}</code>`, `<span class="wrap">${diffLists(c.before || [], c.after || [])}</span>`],
  });
}

async function viewCandidate(id) {
  const [c] = await Promise.all([api(`/api/candidates/${id}`), ctx.config ? Promise.resolve() : loadContext()]);
  const r = c.review || {};
  const gate = r.gate;
  const llm = r.llm;
  const open = ["pending", "deferred", "publish_review", "failed"].includes(c.state);
  const publishFlags = (r.publish_flags || []).map((f) => `<li>${pill("high")} ${esc(f)}</li>`).join("");
  const diffP = r.from || r.to || c.aur_commit ? api(`/api/candidates/${id}/diff`).catch((e) => ({ error: e.message })) : Promise.resolve(null);
  const jobsP = api(`/api/jobs?candidate=${id}`).catch(() => []);
  const [diff, jobs] = await Promise.all([diffP, jobsP]);
  if (location.hash !== `#/candidates/${id}`) return;

  const reasons = (gate?.reasons || []).map((x) => `<li>${pill(x.risk)} <b>${esc(x.kind)}</b> <span class="muted">${esc(x.detail)}</span></li>`).join("");
  const findings = (gate?.findings || []).map((f) => {
    const where = f.function ? `${f.file}:${f.line} ${f.function}()` : `${f.file}:${f.line}`;
    return `<li>${pill(f.severity)} <b>${esc(f.rule)}</b> <span class="muted">${esc(where)}</span><br><code>${esc(f.excerpt)}</code>${f.note ? ` <span class="muted">(${esc(f.note)})</span>` : ""}</li>`;
  }).join("");
  let llmHtml = "";
  if (llm) {
    const v = llm.verdict;
    llmHtml = `<h2>LLM review</h2><div class="panel">
      <div class="muted">${esc(llm.model)}${llm.served_by && llm.served_by !== llm.model ? ` served by ${esc(llm.served_by)}` : ""} · $${(llm.cost_usd || 0).toFixed(4)} · ${llm.usage?.input_tokens ?? "?"} in / ${llm.usage?.output_tokens ?? "?"} out · ${ago(llm.reviewed_at)}</div>
      ${llm.error ? `<p><b>No verdict:</b> ${esc(llm.error)}</p>` : ""}
      ${llm.refused ? `<p><b>Refused</b> (${esc(llm.refused)}); no verdict.</p>` : ""}
      ${v ? `<p>${pill(v.risk)} ${esc(v.summary)}</p>
        <ul class="findings">${(v.findings || []).map((f) => `<li>${pill(f.severity)} <b>${esc(f.kind)}</b> ${esc(f.evidence)}</li>`).join("")}</ul>
        ${(v.questions_for_human || []).length ? `<p><b>For you to check:</b></p><ul>${v.questions_for_human.map((q) => `<li>${esc(q)}</li>`).join("")}</ul>` : ""}` : ""}
      ${r.comments ? `<p class="muted">${r.comments.error ? `AUR comments unavailable: ${esc(r.comments.error)}` : `${r.comments.shown} AUR comment${r.comments.shown === 1 ? "" : "s"} shown to the model`}${r.comments.url ? ` · <a href="${esc(r.comments.url)}" target="_blank" rel="noopener">package page</a>` : ""}</p>` : ""}
      ${llm.prompt ? `<details data-key="prompt"><summary>what the model saw (${bytes(llm.prompt.length)} of prompt, sha256 ${esc(short(llm.prompt_sha256, 12))})</summary><pre class="diff">${esc(llm.prompt)}</pre></details>` : ""}
    </div>`;
  }
  const commits = (r.commits || []).map((k) => `<tr><td class="mono">${esc(short(k.hash))}</td><td>${ago(k.date)}</td><td>${esc(k.author)}</td><td>${esc(k.subject)}</td></tr>`).join("");
  const scripts = (diff?.files || []).map((f) => `<h2>${esc(f.name)} <span class="pill">${esc(f.status)}</span></h2><pre class="diff">${f.diff ? renderDiff(f.diff) : esc(f.text || "")}</pre>`).join("");
  const classes = ["upstream-binary", "upstream-source", "vcs", "repackaged", "authored"];

  // The decision line: what happens if you do nothing.
  let decision = "";
  if (c.state === "pending" || c.state === "deferred") {
    const why = needsHumanBecause(c);
    if (why.length) decision = `<div class="panel callout"><b>Needs you.</b> ${esc(why.join(". "))}.</div>`;
    else {
      const ends = quarantineEnds(c);
      decision = `<div class="panel callout low"><b>The gate will approve this on its own</b> ${ends && Date.parse(ends) > Date.now() ? `<span title="${esc(absolute(ends))}">${relative(ends)}</span>, when it has been on the AUR for ${quarantineDays(c.pkgbase)} days` : "at the next run"}, unless a signal appears first. Approve to skip the wait; reject to drop this commit.</div>`;
    }
  } else if (c.state === "publish_review") {
    decision = `<div class="panel callout"><b>Built and held.</b> The artifacts differ from what the stream serves in ways that matter on the installing system (below). Approve to rebuild and publish; reject to drop this commit.</div>`;
  } else if (c.state === "failed") {
    decision = `<div class="panel callout"><b>Build failed.</b> See the builds below. Approve to retry at the next run; reject to drop this commit; fix the policy first if the log says why.</div>`;
  }
  const builtVersions = [...new Set(jobs.flatMap((j) => (j.result?.artifacts || []).map((a) => a.pkgver)))];

  swap(`
    <div class="row between">
      <h1><a href="#/packages/${esc(c.pkgbase)}">${esc(c.pkgbase)}</a> <span class="muted">${esc(c.kind)}</span> ${pill(c.state)}</h1>
      ${open ? `<div class="row" id="decide">
        ${c.kind === "onboarding" || !c.class ? `<select id="class"><option value="">class…</option>${classes.map((k) => `<option ${k === c.class ? "selected" : ""}>${k}</option>`).join("")}</select>` : ""}
        <input id="note" placeholder="note (optional)" size="32">
        <button class="primary" data-decision="approve">Approve</button>
        <button data-decision="defer">Defer</button>
        <button class="danger" data-decision="reject">Reject</button>
      </div>` : `<div class="muted">decided by ${dash(c.decided_by)} ${ago(c.decided_at)}${c.note ? ` — ${esc(c.note)}` : ""}</div>`}
    </div>
    ${decision}
    <div class="panel">${kv([
      ["version", `${versionChange(r) || dash(r.version_to)}${builtVersions.length ? ` <span class="muted">built ${esc(builtVersions.join(", "))}</span>` : ""}`],
      ["commit", `<span class="mono">${r.from ? `${short(r.from)} → ` : ""}${short(c.aur_commit)}</span>${(r.commits || []).length ? ` <span class="muted">${r.commits.length} commit${r.commits.length > 1 ? "s" : ""}</span>` : ""}`],
      r.upstream_rev ? ["upstream", `<span class="mono">${esc(r.upstream_rev)}</span>`] : null,
      ["class", dash(c.class)],
      ["gate", `${pill(gate?.diff_class)} floor ${pill(c.floor_risk)} llm ${pill(c.llm_risk)}`],
      ["first seen on AUR", `${agoEpoch(c.first_seen_last_modified)} <span class="muted">(quarantine clock, ${quarantineDays(c.pkgbase)}d)</span>`],
      ["seen by repoman", `${ago(c.first_seen_at)} <span class="muted">in run <a href="#/runs/${esc(c.run_id)}">${short(c.run_id)}</a></span>`],
      r.gate_error ? ["gate error", esc(r.gate_error)] : null,
      r.corroboration ? ["upstream check", `${r.corroboration.result === "confirmed" ? pill("low", "confirmed") : r.corroboration.result === "mismatch" ? pill("high", "mismatch") : pill("medium", "unconfirmed")} <span class="muted">${esc(r.corroboration.detail)}</span>`] : null,
      r.note ? ["note", esc(r.note)] : null,
    ])}</div>
    ${publishFlags ? `<h2>Artifact flags</h2><ul class="findings">${publishFlags}</ul>` : ""}
    ${(c.signals || []).length ? `<h2>External signals</h2><p class="muted">Anything above <i>info</i> that is not cleared or expired blocks auto-approval of this package. Clear a signal only after you have looked at what it points to.</p>${signalsList(c.signals)}` : ""}
    ${r.changes?.length ? `<h2>SRCINFO changes</h2>${changesTable(r.changes)}` : ""}
    ${reasons ? `<h2>Reasons</h2><ul class="findings">${reasons}</ul>` : ""}
    ${findings ? `<h2>Lint findings</h2><ul class="findings">${findings}</ul>` : ""}
    ${llmHtml}
    ${jobs.length ? `<h2>Builds</h2>${jobsTable(jobs)}${artifactsTable(jobs)}` : ""}
    ${diff?.error ? `<p class="muted">diff unavailable: ${esc(diff.error)}</p>` : ""}
    ${diff?.pkgbuild != null ? `<h2>PKGBUILD ${r.from ? "diff" : ""}</h2><pre class="diff" id="pkgbuild">${r.from ? renderDiff(diff.pkgbuild) : esc(diff.pkgbuild)}</pre>` : ""}
    ${scripts}
    ${commits ? `<h2>Commits</h2><table><tbody>${commits}</tbody></table>` : ""}
  `);
  wireSignalClears();
  document.querySelectorAll("#decide button").forEach((b) => b.onclick = async () => {
    const decision = b.dataset.decision;
    const body = { decision, note: document.getElementById("note").value || null };
    const cls = document.getElementById("class")?.value;
    if (cls) body.class = cls;
    document.querySelectorAll("#decide button").forEach((x) => x.disabled = true);
    try { const res = await post(`/api/candidates/${id}/decide`, body); toast(`${res.pkgbase} ${res.state}`); location.hash = "#/queue"; }
    catch (e) { toast(e.message, true); document.querySelectorAll("#decide button").forEach((x) => x.disabled = false); }
  });
}

// ---- runs ----------------------------------------------------------------------

function jobResultCell(j) {
  if (j.error) return `<span class="ellipsis" title="${esc(j.error)}">${esc(j.error.split(" | ").slice(-1)[0])}</span>`;
  const r = j.result;
  if (!r) return j.exit_code === 0 ? "ok" : dash(j.exit_code);
  if (r.artifacts) return `${r.artifacts.length} artifact${r.artifacts.length === 1 ? "" : "s"}${r.net_attempts ? `, <span class="${j.job?.allow_network ? "muted" : "medium"}">${r.net_attempts} net</span>` : ""}`;
  if (r.revisions) return `<span class="mono" title="${esc(r.revisions.map((x) => `${x[0]} ${x[1]}`).join("\n"))}">${esc(r.revisions.map((x) => short(x[1], 10)).join("+"))}</span>`;
  if (r.sha256) return `image ${short(r.sha256, 10)}, ${bytes(r.size)}`;
  if (r.srcinfo !== undefined) return "srcinfo regenerated";
  return "ok";
}
function jobsTable(jobs) {
  return table(["job", "kind", "package", "stream", "state", "worker", "queued", "took", "peak", "result"], jobs, {
    href: (j) => `#/jobs/${j.id}`,
    cells: (j) => [
      `<span class="mono">${short(j.id)}</span>${j.attempts > 1 ? ` <span class="pill" title="attempts">×${j.attempts}</span>` : ""}`, esc(j.job?.kind), dash(j.pkgbase), dash(j.stream), pill(j.state), dash(j.worker),
      ago(j.queued_at), j.started_at ? dur(j.started_at, j.finished_at) : "-",
      j.peaks?.peak_rss_bytes ? `<span title="cpu ${Math.round(j.peaks.cpu_seconds || 0)} s, scratch ${bytes(j.peaks.peak_scratch_bytes)}">${bytes(j.peaks.peak_rss_bytes)}</span>` : "-",
      jobResultCell(j),
    ],
    empty: "no jobs",
  });
}

async function viewRuns() {
  const runs = await api("/api/runs");
  swap(`
    <div class="row between"><h1>Runs</h1><button class="primary" id="start-run">Start run</button></div>
    ${table(["run", "state", "step", "started", "took", "planned", "ok", "failed", "published", "held", "skipped"], runs, {
      href: (r) => `#/runs/${r.id}`,
      cells: (r) => [
        `<span class="mono">${short(r.id)}</span>`, pill(r.state), r.state === "running" ? progressLine(r) : dash(r.step), ago(r.started_at),
        dur(r.started_at, r.finished_at), dash(r.summary?.builds_planned), dash(r.summary?.builds_succeeded), r.summary?.builds_failed ? `<span class="pill high">${r.summary.builds_failed}</span>` : dash(r.summary?.builds_failed), dash(r.summary?.published), dash(r.summary?.held_for_review), r.summary?.skipped?.length ? `<span class="pill medium">${r.summary.skipped.length}</span>` : dash(r.summary?.skipped?.length),
      ],
      empty: "no runs yet",
    })}`);
  document.getElementById("start-run").onclick = async (e) => {
    e.target.disabled = true;
    try { const r = await post("/api/runs"); location.hash = `#/runs/${r.id}`; }
    catch (err) { toast(err.message, true); e.target.disabled = false; }
  };
}
viewRuns.refresh = 5000;

// Steps of a run from its run.step events, with how long each took.
function stepTimeline(r) {
  const steps = (r.events || []).filter((e) => e.kind === "run.step").sort((a, b) => a.seq - b.seq);
  if (!steps.length) return "";
  const end = r.finished_at || new Date().toISOString();
  return `<div class="timeline">${steps.map((s, i) => {
    const next = steps[i + 1]?.ts || end;
    const live = !steps[i + 1] && !r.finished_at;
    return `<span class="step ${live ? "live" : ""}" title="${esc(absolute(s.ts))}">${esc(s.payload.step)} <span class="muted">${dur(s.ts, next)}</span></span>`;
  }).join('<span class="muted"> › </span>')}</div>`;
}

async function viewRun(id) {
  const r = await api(`/api/runs/${id}`);
  if (location.hash !== `#/runs/${id}`) return;
  const open = r.state === "running" || r.state === "needs_input";
  const s = r.summary || {};
  const hasSummary = Object.keys(s).length > 0;
  const cands = table(["package", "kind", "state", "change", "diff", "floor", "llm", "decided by", "notes"], r.candidates, {
    href: (c) => `#/candidates/${c.id}`,
    cells: (c) => [esc(c.pkgbase), esc(c.kind), pill(c.state), versionChange(c.review || {}) || "-", pill(c.review?.gate?.diff_class), pill(c.floor_risk), pill(c.llm_risk), dash(c.decided_by), `<span class="ellipsis">${esc(candidateSummary(c))}</span>`],
    empty: "no candidates in this run",
  });
  const jobs = [...r.jobs].sort((a, b) => (b.queued_at > a.queued_at ? 1 : -1));
  const byKind = {};
  for (const j of jobs) byKind[j.job?.kind] = (byKind[j.job?.kind] || 0) + 1;
  const probes = jobs.filter((j) => j.job?.kind === "probe");
  const probeFails = probes.filter((j) => j.state === "failed" || j.error);
  swap(`
    <div class="row between">
      <h1>Run <span class="mono">${short(id)}</span> ${pill(r.state)} ${r.step ? `<span class="muted">step ${esc(r.step)}</span>` : ""}</h1>
      ${r.state === "needs_input" ? `<a class="btn" href="#/queue">Decide pending candidates</a>` : ""}
    </div>
    <div class="muted">started ${ago(r.started_at)} · ${open ? "running for" : "took"} ${dur(r.started_at, r.finished_at)}${r.finished_at ? ` · finished ${ago(r.finished_at)}` : ""}</div>
    ${r.state === "running" ? `<div class="panel">${progressLine(r)}</div>` : ""}
    ${stepTimeline(r)}
    ${hasSummary ? `<div class="cards">
      <div class="card"><div class="label">builds</div><div class="big">${s.builds_succeeded ?? 0}<span class="muted"> / ${s.builds_planned ?? 0}</span></div><div class="muted">${s.builds_failed ? `<span class="high">${s.builds_failed} failed</span>` : "none failed"} · ${(s.streams || []).join(", ") || "no streams"}</div></div>
      <div class="card"><div class="label">published</div><div class="big">${s.published ?? 0}</div><div class="muted">${s.held_for_review ? `${s.held_for_review} held for artifact review` : "nothing held"}</div></div>
      <div class="card"><div class="label">skipped</div><div class="big">${(s.skipped || []).length}</div><div class="muted">${(s.skipped || []).length ? "could not be planned; see below" : "everything planned was built"}</div></div>
      <div class="card"><div class="label">jobs</div><div class="big">${jobs.length}</div><div class="muted">${Object.entries(byKind).map(([k, n]) => `${n} ${k}`).join(", ") || "none"}${probeFails.length ? ` · <span class="high">${probeFails.length} probe${probeFails.length > 1 ? "s" : ""} failed</span>` : ""}</div></div>
    </div>` : ""}
    ${(s.skipped || []).length ? `<h2>Skipped</h2>${table(["package", "stream", "reason"], s.skipped, { href: (x) => `#/packages/${x.pkgbase}`, cells: (x) => [esc(x.pkgbase), dash(x.stream), `<span class="wrap">${esc(x.reason)}</span>`] })}` : ""}
    <h2>Candidates (${r.candidates.length})</h2>${cands}
    <h2>Jobs (${jobs.length})</h2>${jobsTable(jobs)}`);
}
viewRun.refresh = 4000;

// ---- job -------------------------------------------------------------------------

function logLineHtml(l) {
  const cls = l.fd === "system" ? (l.line.startsWith("net:") ? "net" : "sys") : l.fd === "stderr" ? "err" : "";
  return `<span class="${cls}" title="${esc(absolute(l.ts))}">${esc(l.line)}</span>\n`;
}
function jobSpecPanel(j) {
  const s = j.job || {};
  switch (s.kind) {
    case "build": return kv([
      ["image", `<span class="mono">${short(s.image?.sha256, 12)}</span> <span class="muted">${esc(s.image?.filename || "")}</span>`],
      ["commit", `<span class="mono">${esc(s.aur_commit)}</span>`],
      ["installed first", `<details data-key="install"><summary>${s.install?.length || 0} packages (resolved depends, makedepends, checkdepends)</summary><div class="wrap mono small">${(s.install || []).map(esc).join(" ")}</div></details>`],
      ["memory limit", s.memory_limit ? bytes(s.memory_limit) : "none"],
      ["network", s.allow_network ? '<span class="pill medium">allowed during build (policy)</span>' : "off after sources are fetched; attempts are counted"],
      ["pgp", s.skip_pgp ? '<span class="pill medium">verification skipped (policy)</span>' : `${(s.pgp_keys || []).length} key${(s.pgp_keys || []).length === 1 ? "" : "s"} trusted`],
    ]);
    case "probe": return kv([["sources", `<div class="wrap mono small">${(s.sources || []).map(esc).join("<br>")}</div>`]]);
    case "image": return kv([["stream", esc(s.stream)], ["packages", `<details data-key="imgpkgs"><summary>${s.packages?.length || 0} packages</summary><div class="wrap mono small">${(s.packages || []).map(esc).join(" ")}</div></details>`], ["keyrings", listOrNone(s.keyrings)]]);
    case "inspect": return kv([["commit", `<span class="mono">${esc(s.aur_commit)}</span>`], ["image", `<span class="mono">${short(s.image?.sha256, 12)}</span>`]]);
    default: return "";
  }
}
function jobResultPanel(j) {
  const r = j.result;
  if (!r) return "";
  if (r.artifacts) {
    const arts = table(["file", "package", "version", "size", "depends", "setuid"], r.artifacts, {
      cells: (a) => [`<span class="mono">${esc(a.filename)}</span>`, esc(a.pkgname), esc(a.pkgver), bytes(a.size), `<span class="wrap small">${listOrNone(a.depends, 12)}</span>`, (a.setuid_paths || []).length ? `<span class="pill high">${esc(a.setuid_paths.join(" "))}</span>` : "none"],
      empty: "no artifacts",
    });
    return `${kv([
      r.upstream_rev ? ["upstream revision", `<span class="mono">${esc(r.upstream_rev)}</span>`] : null,
      ["network attempts", r.net_attempts ? `<span class="pill ${j.job?.allow_network ? "" : "medium"}">${r.net_attempts}</span> <span class="muted">${j.job?.allow_network ? "network was allowed" : "rejected packets; the net: lines in the log say where they went"}</span>` : "0"],
      ["outside writes", (r.outside_writes || []).length ? `<span class="pill medium">${r.outside_writes.length}</span> <span class="wrap mono small">${r.outside_writes.map(esc).join(" ")}</span>` : "none"],
    ])}${arts}`;
  }
  if (r.revisions) return table(["source", "upstream revision"], r.revisions, { cells: (x) => [`<span class="wrap mono small">${esc(x[0])}</span>`, `<span class="mono">${esc(x[1])}</span>`] });
  if (r.sha256) return kv([["image", `<span class="mono">${esc(r.sha256)}</span>`], ["size", bytes(r.size)], ["packages", `<details data-key="imgpkgs"><summary>${r.packages?.length || 0} installed</summary><div class="wrap mono small">${(r.packages || []).map(esc).join("<br>")}</div></details>`]]);
  if (r.srcinfo !== undefined) return `<details data-key="srcinfo"><summary>regenerated .SRCINFO</summary><pre>${esc(r.srcinfo)}</pre></details>`;
  return "";
}

const logState = { id: null, lastSeq: -1, count: 0 };
async function viewJob(id) {
  const again = $main.dataset.view === `job:${id}` && document.getElementById("log");
  const after = again ? logState.lastSeq : -1;
  const [j, log] = await Promise.all([api(`/api/jobs/${id}`), api(`/api/jobs/${id}/log${after >= 0 ? `?after=${after}` : ""}`)]);
  if (location.hash !== `#/jobs/${id}`) return;
  const live = ["queued", "assigned", "running"].includes(j.state);
  const stages = (j.stages || []).map((s) => `<tr><td>${esc(s.stage)}</td><td>${ago(s.started_at)}</td><td>${s.finished_at ? dur(s.started_at, s.finished_at) : pill("running")}</td></tr>`).join("");
  const netLines = log.filter((l) => l.fd === "system" && l.line.startsWith("net:")).length;
  const head = `
    <h1>Job <span class="mono">${short(id)}</span> ${pill(j.state)} <span class="muted">${esc(j.job?.kind)} ${j.pkgbase ? `<a href="#/packages/${esc(j.pkgbase)}">${esc(j.pkgbase)}</a>` : ""} ${dash(j.stream)}</span></h1>
    <div class="panel">${kv([
      ["worker", `${dash(j.worker)}${j.attempts > 1 ? ` <span class="pill medium">attempt ${j.attempts}</span>` : ""}`],
      ["timing", `queued ${ago(j.queued_at)}${j.started_at ? `, started ${ago(j.started_at)}` : ""}${j.finished_at ? `, finished ${ago(j.finished_at)}` : ""}${j.started_at ? ` · ran ${dur(j.started_at, j.finished_at)}` : ""}${j.assigned_at && j.started_at ? ` <span class="muted">(${dur(j.queued_at, j.started_at)} in the queue)</span>` : ""}`],
      ["exit", `${j.exit_code === 0 ? pill("done", "0") : dash(j.exit_code)}${j.error ? ` <span class="high">${esc(j.error)}</span>` : ""}`],
      j.peaks?.peak_rss_bytes || j.job?.memory_limit ? ["memory", `${j.peaks?.peak_rss_bytes ? `peak ${bytes(j.peaks.peak_rss_bytes)}` : ""}${j.job?.memory_limit ? ` <span class="muted">of ${bytes(j.job.memory_limit)} allowed${j.peaks?.peak_rss_bytes ? ` (${Math.round(100 * j.peaks.peak_rss_bytes / j.job.memory_limit)}%)` : ""}</span>` : ""}`] : null,
      j.peaks?.cpu_seconds || j.peaks?.peak_scratch_bytes ? ["cpu / scratch", `${j.peaks.cpu_seconds ? `${Math.round(j.peaks.cpu_seconds)} s cpu` : ""}${j.peaks.peak_scratch_bytes ? `, ${bytes(j.peaks.peak_scratch_bytes)} scratch` : ""}`] : null,
      ["belongs to", `${j.candidate_id ? `candidate <a href="#/candidates/${esc(j.candidate_id)}" class="mono">${short(j.candidate_id)}</a>` : ""}${j.run_id ? `${j.candidate_id ? " · " : ""}run <a href="#/runs/${esc(j.run_id)}" class="mono">${short(j.run_id)}</a>` : ""}` || '<span class="muted">manual</span>'],
    ])}</div>
    <h2>Spec</h2><div class="panel">${jobSpecPanel(j) || '<span class="muted">nothing to show</span>'}</div>
    ${j.result ? `<h2>Result</h2><div class="panel">${jobResultPanel(j)}</div>` : ""}
    ${stages ? `<h2>Stages</h2><table><tbody>${stages}</tbody></table>` : ""}`;
  // The log is the one thing not rendered from scratch: the <pre> keeps
  // its lines across morphs (data-morph-skip) and only new ones are
  // appended, following the tail when the reader is at it.
  if (!again) { logState.id = id; logState.count = 0; logState.lastSeq = -1; }
  const count = logState.count + log.length;
  swap(`
    <div id="job-head">${head}</div>
    <h2>Log <span id="log-count" class="muted">${count} lines</span>${netLines ? ` <span class="pill medium">${netLines} net</span>` : ""} <span id="log-live">${live ? '<span class="pill running">live</span>' : ""}</span></h2>
    <pre class="log" id="log" data-morph-skip>${again ? "" : '<span class="muted">no output yet</span>'}</pre>`);
  const pre = document.getElementById("log");
  if (log.length) {
    const atBottom = again && pre.scrollHeight - pre.scrollTop - pre.clientHeight < 4;
    if (logState.count === 0) pre.innerHTML = "";
    pre.insertAdjacentHTML("beforeend", log.map(logLineHtml).join(""));
    logState.count = count;
    logState.lastSeq = log[log.length - 1].seq;
    if (!again) pre.scrollTop = live ? pre.scrollHeight : 0;
    else if (atBottom) pre.scrollTop = pre.scrollHeight;
  }
  $main.dataset.view = `job:${id}`;
}
viewJob.refresh = 3000;

// ---- packages --------------------------------------------------------------------

function policySummary(p) {
  const keys = Object.keys(p.policy || {});
  if (!keys.length) return "";
  return `<span class="muted" title="${esc(JSON.stringify(p.policy))}">${keys.map(esc).join(", ")}</span>`;
}
function isBehind(p) {
  if (p.kind === "vcs" || !p.snapshot?.version || !p.live?.length) return false;
  return p.live.some((v) => v.pkgver !== p.snapshot.version);
}
async function viewPackages() {
  const { packages: pkgs } = await loadContext();
  if (location.hash !== "#/packages") return;
  const q = (document.getElementById("filter")?.value || "").toLowerCase();
  const matches = (p, q) => {
    if (!q) return true;
    const specials = { behind: () => isBehind(p), orphan: () => p.snapshot && !p.snapshot.maintainer && !p.source_url, signal: () => (p.signals || []).some((s) => sigActive(s) && s.severity !== "info"), open: () => p.open_candidates.length > 0, gone: () => !!p.snapshot?.missing_since, stale: () => !!p.snapshot?.out_of_date };
    return q.split(/\s+/).every((t) => specials[t] ? specials[t]() : p.pkgbase.includes(t) || (p.provenance_class || "").includes(t) || p.kind.includes(t) || p.streams.some((s) => s.stream.includes(t)) || (p.snapshot?.maintainer || "").toLowerCase().includes(t));
  };
  const render = () => {
    const q2 = (document.getElementById("filter")?.value || "").toLowerCase();
    const rows2 = pkgs.filter((p) => matches(p, q2));
    document.getElementById("pkg-table").innerHTML = table(["package", "class", "serving", "AUR", "maintainer", "state", "open", "policy"], rows2, {
      href: (p) => `#/packages/${p.pkgbase}`,
      cls: "compact",
      cells: (p) => [
        `${esc(p.pkgbase)} <span class="muted small">${esc(p.kind)}${p.streams.some((s) => s.required && !s.wanted) ? " · dep" : ""}</span>`,
        `<span class="nowrap">${dash(p.provenance_class)}</span>`,
        liveCell(p),
        `${dash(p.snapshot?.version)}${p.snapshot?.last_modified ? ` <span class="muted small">${relative(new Date(p.snapshot.last_modified * 1000).toISOString())}</span>` : ""}`,
        p.source_url ? '<span class="muted">git source</span>' : `${p.snapshot ? (p.snapshot.maintainer ? esc(p.snapshot.maintainer) : '<span class="muted">nobody</span>') : "-"}${p.snapshot?.votes != null ? ` <span class="muted small">${p.snapshot.votes}</span>` : ""}`,
        aurStatePills(p),
        p.open_candidates.length ? `<a href="#/candidates/${p.open_candidates[0].id}">${pill(p.open_candidates[0].state)}</a>` : "",
        policySummary(p),
      ],
      empty: "no packages match",
    });
    document.getElementById("pkg-count").textContent = rows2.length === pkgs.length ? `(${pkgs.length})` : `(${rows2.length} of ${pkgs.length})`;
  };
  const behind = pkgs.filter(isBehind).length;
  swap(`
    <div class="row between">
      <h1>Packages <span class="muted" id="pkg-count">(${pkgs.length})</span></h1>
      <div class="row">
        <input id="filter" placeholder="filter: name, class, stream, maintainer, behind, orphan, signal, open, gone, stale" size="52" value="${esc(q)}">
        <input id="add-name" placeholder="pkgbase to add" size="18">
        <select id="add-stream">${(ctx.config?.streams || []).map((s) => `<option value="${esc(s)}">${esc(s.replace("manjaro-", ""))}</option>`).join("")}</select>
        <button id="add">Add</button>
      </div>
    </div>
    ${behind ? `<p class="muted">${behind} package${behind > 1 ? "s are" : " is"} behind the AUR (<a href="#" id="show-behind">show</a>).</p>` : ""}
    <div id="pkg-table"></div>`);
  render();
  document.getElementById("filter").oninput = render;
  document.getElementById("show-behind")?.addEventListener("click", (e) => { e.preventDefault(); document.getElementById("filter").value = "behind"; render(); });
  document.getElementById("add").onclick = async () => {
    const pkgbase = document.getElementById("add-name").value.trim();
    const stream = document.getElementById("add-stream").value;
    if (!pkgbase) return;
    try { await post("/api/packages", { pkgbase, streams: [stream] }); toast(`${pkgbase} added to ${stream}`); location.hash = `#/packages/${pkgbase}`; }
    catch (e) { toast(e.message, true); }
  };
}

function eventLine(e) {
  const p = e.payload || {};
  const text = {
    "candidate.auto_approved": () => `gate approved a ${p.class} change`,
    "candidate.decided": () => `${p.state || "decided"}${p.by ? ` by ${p.by}` : ""}${p.note ? `: ${p.note}` : ""}`,
    "candidate.publish_review": () => `build held for artifact review: ${(p.flags || []).join(", ")}`,
    "candidate.soname_rebuild": () => `rebuild queued on ${p.stream}: ${(p.sonames || []).join(", ")} changed`,
    "repo.published": () => `repository updated: ${p.count} added, ${p.retired} retired`,
    "aur.maintainer_changed": () => `maintainer ${p.from || "nobody"} → ${p.to || "nobody"}`,
    "aur.adopted": () => `adopted by ${p.by}`,
    "aur.orphaned": () => `orphaned by ${p.from}`,
    "aur.missing": () => "not listed by the AUR RPC",
    "aur.reappeared": () => "back on the AUR",
    "aur.deleted": () => "deleted from the AUR",
    "aur.resubmitted": () => `resubmitted (base id ${p.old_id} → ${p.new_id})`,
    "aur.history_rewritten": () => "git history replaced",
    "aur_requests.seen": () => `${p.request} request #${p.prq}${p.outcome ? ` ${p.outcome}` : ""} by ${p.who || "?"}`,
    "aur_general.mention": () => `mentioned on aur-general: ${p.subject}`,
    "signal.cleared": () => `signal cleared by ${p.by}${p.note ? `: ${p.note}` : ""}`,
    "package.added": () => `${p.created === false ? "membership added" : "added"}${p.streams?.length ? ` to ${[].concat(p.streams).join(", ")}` : ""}`,
    "package.discovered": () => `pulled in as dependency ${p.dep} of ${p.needed_by}`,
    "package.removed": () => "removed",
    "package.membership_removed": () => `removed from ${p.stream}`,
    "job.requeued": () => `job requeued${p.reason ? `: ${p.reason}` : ""}`,
    "run.step": () => `step ${p.step}`,
  }[e.kind];
  return text ? esc(text()) : `<code class="ellipsis">${esc(JSON.stringify(p))}</code>`;
}

async function viewPackage(pkgbase) {
  const [p, h, events] = await Promise.all([api(`/api/packages/${pkgbase}`), api(`/api/packages/${pkgbase}/history`), api(`/api/events?subject=${encodeURIComponent(pkgbase)}&limit=40`), ctx.config ? Promise.resolve() : loadContext()]);
  if (location.hash !== `#/packages/${pkgbase}`) return;
  const s = p.snapshot || {};
  const cands = table(["when", "kind", "state", "change", "diff", "floor", "llm", "decided by", "notes"], h.candidates, {
    href: (c) => `#/candidates/${c.id}`,
    cells: (c) => [ago(c.first_seen_at), esc(c.kind), pill(c.state), versionChange(c.review || {}) || "-", pill(c.review?.gate?.diff_class), pill(c.floor_risk), pill(c.llm_risk), dash(c.decided_by), `<span class="ellipsis">${esc(candidateSummary(c))}</span>`],
    empty: "no candidates yet",
  });
  const pubs = table(["repo", "package", "version", "published", "retired", "size", "commit", "upstream", "job"], h.publications, {
    cells: (x) => [esc(x.repo.replace("manjaro-", "")), esc(x.artifact.pkgname), esc(x.artifact.pkgver), ago(x.published_at), x.retired_at ? ago(x.retired_at) : pill("live"), bytes(x.artifact.size), `<span class="mono">${short(x.artifact.aur_commit, 8)}</span>`, `<span class="mono">${short(x.artifact.upstream_rev, 12)}</span>`, x.artifact.job_id ? `<a href="#/jobs/${esc(x.artifact.job_id)}" class="mono">${short(x.artifact.job_id)}</a>` : "-"],
    empty: "nothing published yet",
  });
  const serving = p.live?.length ? liveCell(p) : '<span class="muted">nothing served yet</span>';
  swap(`
    <div class="row between">
      <h1>${esc(p.pkgbase)} <span class="muted">${esc(p.kind)} · ${dash(p.provenance_class)}</span> ${aurStatePills(p)}</h1>
      <div class="row">
        <a class="btn" href="${p.source_url ? esc(p.source_url.split("#")[0]) : `https://aur.archlinux.org/pkgbase/${esc(p.pkgbase)}`}" target="_blank" rel="noopener">${p.source_url ? "source repo" : "AUR page"}</a>
        <button class="danger" id="remove">Remove package</button>
      </div>
    </div>
    <div class="panel">${kv([
      ["serving", serving],
      ["streams", p.streams.map((m) => `<span class="pill">${esc(m.stream)}${m.required && !m.wanted ? " (dependency)" : ""}</span>`).join(" ") || "-"],
      p.source_url
        ? ["upstream", `${dash(s.version)} <span class="muted">head ${short(s.head_commit)}, committed ${agoEpoch(s.last_modified)}, fetched ${ago(s.fetched_at)}</span>`]
        : ["AUR version", `${dash(s.version)} <span class="muted">modified ${agoEpoch(s.last_modified)}, fetched ${ago(s.fetched_at)}${s.first_submitted ? `, on the AUR since ${agoEpoch(s.first_submitted)}` : ""}</span>`],
      p.source_url ? null : ["maintainer", `${s.maintainer ? esc(s.maintainer) : '<span class="muted">nobody</span>'}${(s.comaintainers || []).length ? ` <span class="muted">+ ${esc(s.comaintainers.join(", "))}</span>` : ""}${s.submitter && s.submitter !== s.maintainer ? ` <span class="muted">submitted by ${esc(s.submitter)}</span>` : ""}`],
      p.source_url ? null : ["votes", `${dash(s.votes)} <span class="muted">popularity ${s.popularity != null ? s.popularity.toFixed(2) : "-"}</span>`],
      ["baseline", p.baseline ? `<span class="mono">${short(p.baseline.aur_commit)}</span> published ${ago(p.baseline.published_at)}${p.baseline.maintainers?.length ? ` by ${esc(p.baseline.maintainers.join(", "))}` : ""}` : '<span class="muted">none yet</span>'],
      p.source_url ? ["source", `<span class="mono">${esc(p.source_url)}</span>`] : null,
      ["added", ago(p.added_at)],
      ["quarantine", `${quarantineDays(p.pkgbase)} days${(p.policy || {}).quarantine_days != null ? " (policy)" : ""}`],
    ])}</div>
    ${(p.signals || []).length ? `<h2>Signals</h2>${signalsList(p.signals)}` : ""}
    <h2>Candidates</h2>${cands}
    <h2>Publications</h2>${pubs}
    <h2>Policy</h2>
    <div class="panel">
      <textarea class="mono" id="policy">${esc(JSON.stringify(p.policy || {}, null, 2))}</textarea>
      <div class="row" style="margin-top:8px">
        <button class="primary" id="save-policy">Save policy</button>
        <span class="muted">keys: pgp="skip", allow_network_build, build_once, provider_choices, extra_depends, quarantine_days, auto_metadata, probe_interval_hours</span>
      </div>
    </div>
    <h2>Events</h2>
    ${table(["when", "what"], events, { cells: (e) => [ago(e.ts), `<span class="muted">${esc(e.kind)}</span> ${eventLine(e)}`], empty: "no events recorded" })}`);
  wireSignalClears();
  document.getElementById("save-policy").onclick = async () => {
    try {
      const policy = JSON.parse(document.getElementById("policy").value);
      await post(`/api/packages/${pkgbase}/policy`, { policy });
      toast("policy saved");
    } catch (e) { toast(e.message, true); }
  };
  const rm = document.getElementById("remove");
  rm.onclick = async () => {
    if (!rm.classList.contains("armed")) { rm.classList.add("armed"); rm.textContent = "Click again to remove"; setTimeout(() => { rm.classList.remove("armed"); rm.textContent = "Remove package"; }, 4000); return; }
    try { await del(`/api/packages/${pkgbase}`); toast(`${pkgbase} removed`); location.hash = "#/packages"; }
    catch (e) { toast(e.message, true); }
  };
}

// ---- system ----------------------------------------------------------------------

async function viewSystem() {
  const [workers, images, events, health, cache, signals, reviews, config] = await Promise.all([api("/api/workers"), api("/api/images"), api("/api/events?limit=60"), api("/api/health"), api("/api/cache"), api("/api/signals"), api("/api/reviews"), api("/api/config")]);
  const { packages: pkgs } = await loadContext();
  if (location.hash !== "#/system") return;
  const aur = pkgs.filter((p) => !p.source_url && p.snapshot);
  const orphans = aur.filter((p) => !p.snapshot.maintainer && !p.snapshot.missing_since);
  const ood = aur.filter((p) => p.snapshot.out_of_date);
  const gone = aur.filter((p) => p.snapshot.missing_since);
  const behind = pkgs.filter(isBehind);
  const names = (l) => l.slice(0, 12).map((p) => `<a href="#/packages/${esc(p.pkgbase)}">${esc(p.pkgbase)}</a>`).join(", ") + (l.length > 12 ? ` <span class="muted">+${l.length - 12}</span>` : "");
  swap(`
    <h1>System <span class="muted">repoman ${esc(health.version)}</span></h1>
    <div class="cards">
      <div class="card"><div class="label">auto-approval</div><div class="big">${config.auto_approve ? "on" : "off"}</div><div class="muted">routine after ${config.quarantine_days}d on the AUR${config.auto_approve_metadata ? ", metadata too" : ""}; adopted orphans wait ${config.adoption_quarantine_days}d</div></div>
      <div class="card"><div class="label">reviewer</div><div class="big">${config.reviewer_enabled ? "on" : "off"}</div><div class="muted">${esc(config.reviewer_model)}${config.reviewer_routine_model ? `; routine via ${esc(config.reviewer_routine_model)}` : "; structural and elevated only"}</div></div>
      <div class="card"><div class="label">behind the AUR</div><div class="big">${behind.length}</div><div class="muted">${names(behind) || "every served package is at its AUR version"}</div></div>
      <div class="card"><div class="label">blocking signals</div><div class="big">${signals.length}</div><div class="muted">${signals.length ? "auto-approval is off for these packages until cleared or expired" : "nothing blocks auto-approval"}</div></div>
    </div>
    ${signals.length ? signalsList(signals, { withPackage: true }) : ""}
    <h2>Workers</h2>
    ${table(["name", "arch", "slots", "memory", "scratch", "status", "running", "capabilities", "first seen", "last seen"], workers, {
      cells: (w) => [esc(w.name), dash(w.arch), `${w.running?.length || 0} / ${w.slots}`, bytes(w.mem_bytes), bytes(w.scratch_bytes), w.connected ? pill("connected") : pill("offline"), w.running.map((j) => `<a href="#/jobs/${esc(j)}" class="mono">${short(j)}</a>`).join(" ") || "-", (w.capabilities || []).join(" "), ago(w.first_seen), ago(w.last_seen)],
      empty: "no workers have connected",
    })}
    <h2>Base images</h2>
    ${table(["stream", "image", "size", "packages", "state", "built", "age"], images, {
      cells: (i) => [esc(i.stream), `<span class="mono">${short(i.sha256, 12)}</span>`, bytes(i.size), i.packages?.length ? `<details data-key="img-${esc(short(i.sha256, 12))}"><summary>${i.packages.length}</summary><div class="wrap mono small">${i.packages.map(esc).join("<br>")}</div></details>` : "-", pill(i.state), ago(i.created_at), (Date.now() - Date.parse(i.created_at)) / 1000 > config.image_max_age_secs ? '<span class="pill medium">stale; rebuilt at next run</span>' : "fresh"],
      empty: "no images yet",
    })}
    <h2>AUR state</h2>
    <div class="cards">
      <div class="card"><div class="label">orphaned</div><div class="big">${orphans.length}</div><div class="muted">${names(orphans) || "none"}</div></div>
      <div class="card"><div class="label">flagged out of date</div><div class="big">${ood.length}</div><div class="muted">${names(ood) || "none"}</div></div>
      <div class="card"><div class="label">gone from the AUR</div><div class="big">${gone.length}</div><div class="muted">${names(gone) || "none"}</div></div>
      <div class="card"><div class="label">tracked</div><div class="big">${pkgs.length}</div><div class="muted">${aur.length} on the AUR, ${pkgs.length - aur.length} from other git sources</div></div>
    </div>
    <h2>LLM reviews</h2>
    <div class="cards">
      <div class="card"><div class="label">reviews</div><div class="big">${reviews.count}</div><div class="muted">$${reviews.cost_usd.toFixed(2)} spent in total</div></div>
      <div class="card"><div class="label">verdicts</div><div class="big">${["low", "medium", "high"].map((k) => `${reviews.recent.filter((c) => c.review?.llm?.verdict?.risk === k).length} ${k}`).join(" · ")}</div><div class="muted">of the last ${reviews.recent.length}; ${reviews.recent.filter((c) => c.review?.llm && !c.review.llm.verdict).length} without a verdict</div></div>
    </div>
    ${table(["when", "package", "kind", "model", "risk", "cost", "summary"], reviews.recent, {
      href: (c) => `#/candidates/${c.id}`,
      cells: (c) => [ago(c.review.llm.reviewed_at), esc(c.pkgbase), esc(c.kind), `<span class="muted">${esc(c.review.llm.served_by || c.review.llm.model)}</span>`, c.review.llm.verdict ? pill(c.review.llm.verdict.risk) : `<span class="pill">${esc(c.review.llm.refused ? "refused" : "error")}</span>`, `$${(c.review.llm.cost_usd || 0).toFixed(4)}`, `<span class="ellipsis">${esc(c.review.llm.verdict?.summary || c.review.llm.error || c.review.llm.refused || "")}</span>`],
      empty: "no reviews yet: nothing structural has reached the model",
    })}
    <h2>Package cache</h2>
    <div class="cards">
      <div class="card"><div class="label">state</div><div class="big">${cache.enabled ? "on" : "off"}</div><div class="muted mono">${esc(cache.url)}</div></div>
      <div class="card"><div class="label">files</div><div class="big">${cache.files}</div><div class="muted">${bytes(cache.bytes)} total</div></div>
      <div class="card"><div class="label">hits / misses</div><div class="big">${cache.hits} / ${cache.misses}</div><div class="muted">${cache.errors} fetch errors since start</div></div>
      <div class="card"><div class="label">evictable</div><div class="big">${cache.unreferenced_files}</div><div class="muted">${bytes(cache.unreferenced_bytes)} past the databases, ${cache.grace_days}d grace${cache.max_bytes ? `, cap ${bytes(cache.max_bytes)}` : ", no size cap"}</div></div>
    </div>
    <h2>Recent events</h2>
    ${table(["when", "kind", "subject", "what"], events, {
      cells: (e) => [ago(e.ts), `<span class="muted">${esc(e.kind)}</span>`, e.subject ? (e.kind.startsWith("run.") ? `<a href="#/runs/${esc(e.subject)}" class="mono">${short(e.subject)}</a>` : e.kind.startsWith("repo.") ? esc(e.subject) : e.kind.startsWith("job.") ? `<a href="#/jobs/${esc(e.subject)}" class="mono">${short(e.subject)}</a>` : e.kind.startsWith("candidate.") && e.payload?.id ? `<a href="#/candidates/${esc(e.payload.id)}">${esc(e.subject)}</a>` : `<a href="#/packages/${esc(e.subject)}">${esc(e.subject)}</a>`) : "-", eventLine(e)],
      empty: "no events",
    })}
    <h2>Client configuration</h2>
    <div class="panel"><p>Add to <code>/etc/pacman.conf</code> above the distribution repos, with the stream for that machine:</p>
    <pre>[repoman]
SigLevel = Required DatabaseRequired
Server = ${esc(location.origin)}/repos/${esc(config.streams[0] || "manjaro-stable")}/$arch</pre>
    <p>Optionally let the distribution repos fetch through this host's cache by adding one line to each of <code>[core]</code>, <code>[extra]</code>, and <code>[multilib]</code>, above their <code>Include</code>:</p>
    <pre>CacheServer = ${esc(location.origin)}/cache</pre>
    <p>Import the signing key once (<code>pacman-key --add</code> does not read stdin): <code>curl -s ${esc(location.origin)}/repos/repoman.asc -o /tmp/repoman.asc &amp;&amp; sudo pacman-key --add /tmp/repoman.asc &amp;&amp; sudo pacman-key --lsign-key &lt;fingerprint&gt;</code></p></div>`);
  wireSignalClears();
}
viewSystem.refresh = 10000;

// ---- boot --------------------------------------------------------------------------

header();
route();
