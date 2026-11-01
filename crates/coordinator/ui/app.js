// repoman operator UI. Plain JS over the JSON API; hash routing; views
// that show live state re-fetch every few seconds while visible.

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
function pill(v) { return v ? `<span class="pill ${esc(v)}">${esc(v)}</span>` : "-"; }
function ago(ts) {
  if (!ts) return "-";
  const s = Math.max(0, (Date.now() - Date.parse(ts)) / 1000);
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}
function agoEpoch(secs) { return secs ? ago(new Date(secs * 1000).toISOString()) : "-"; }
function dur(a, b) {
  if (!a) return "-";
  const s = Math.max(0, (Date.parse(b || new Date().toISOString()) - Date.parse(a)) / 1000);
  return s < 90 ? `${Math.round(s)}s` : `${Math.round(s / 60)}m`;
}
function bytes(n) {
  if (n == null) return "-";
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(i ? 1 : 0)} ${u[i]}`;
}
function toast(msg, isError = false) {
  let t = document.querySelector(".toast");
  if (!t) { t = document.createElement("div"); t.className = "toast"; document.body.appendChild(t); }
  t.textContent = msg; t.style.background = isError ? "var(--high)" : "";
  t.classList.add("show");
  clearTimeout(t._timer);
  t._timer = setTimeout(() => t.classList.remove("show"), isError ? 6000 : 2500);
}
function table(headers, rows, opts = {}) {
  const ths = headers.map((h) => `<th>${esc(h)}</th>`).join("");
  const trs = rows.map((r) => {
    const href = opts.href ? opts.href(r) : null;
    const tds = opts.cells(r).map((c) => `<td>${c}</td>`).join("");
    return href ? `<tr class="click" data-href="${esc(href)}">${tds}</tr>` : `<tr>${tds}</tr>`;
  }).join("");
  return `<table><thead><tr>${ths}</tr></thead><tbody>${trs || `<tr><td colspan="${headers.length}" class="muted">${esc(opts.empty || "nothing here")}</td></tr>`}</tbody></table>`;
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
function candidateSummary(c) {
  const r = c.review || {};
  const parts = [];
  if (r.version_from && r.version_to) parts.push(`${r.version_from} → ${r.version_to}`);
  else if (r.version_to) parts.push(r.version_to);
  const reasons = (r.gate?.reasons || []).filter((x) => x.risk !== "low").map((x) => x.kind);
  if (reasons.length) parts.push([...new Set(reasons)].join(" "));
  const n = (r.gate?.findings || []).filter((f) => f.severity !== "low").length;
  if (n) parts.push(`${n} lint finding${n === 1 ? "" : "s"}`);
  if (r.upstream_rev) parts.push(`upstream ${short(r.upstream_rev, 12)}`);
  if (c.note) parts.push(c.note);
  if (r.note && !parts.length) parts.push(r.note);
  return parts.join("; ");
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
      const render = async () => {
        try { await view(...m.slice(1).map(decodeURIComponent)); }
        catch (e) { $main.innerHTML = `<div class="panel"><b>Error:</b> ${esc(e.message)}</div>`; }
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
  if (tr && !e.target.closest("a,button")) location.hash = tr.dataset.href;
});

async function header() {
  try {
    const [h, pending, workers] = await Promise.all([api("/api/health"), api("/api/candidates"), api("/api/workers")]);
    document.getElementById("queue-count").textContent = pending.length || "";
    const online = workers.filter((w) => w.connected).length;
    $status.textContent = `${h.run_active ? "run active" : "idle"} · ${online}/${workers.length} workers`;
  } catch (e) { $status.textContent = "coordinator unreachable"; }
}
setInterval(header, 5000);

// ---- queue -------------------------------------------------------------------

async function viewQueue() {
  const [pending, deferred, runs] = await Promise.all([
    api("/api/candidates"), api("/api/candidates?state=deferred"), api("/api/runs"),
  ]);
  const active = runs.find((r) => r.state === "running" || r.state === "needs_input");
  const cells = (c) => [
    `<a href="#/packages/${esc(c.pkgbase)}">${esc(c.pkgbase)}</a>`, esc(c.kind), dash(c.class),
    pill(c.review?.gate?.diff_class), pill(c.floor_risk), pill(c.llm_risk),
    `<span class="muted">${agoEpoch(c.first_seen_last_modified)}</span>`,
    `<span class="ellipsis">${esc(candidateSummary(c))}</span>`,
  ];
  const headers = ["package", "kind", "class", "diff", "floor", "llm", "on AUR", "summary"];
  $main.innerHTML = `
    <div class="row between">
      <h1>Queue</h1>
      <div class="row">
        ${active ? `<a href="#/runs/${active.id}">run ${short(active.id)} ${pill(active.state)}</a>` : `<span class="muted">no run active</span>`}
        <button class="primary" id="start-run">${active?.state === "needs_input" ? "Resume run" : "Start run"}</button>
      </div>
    </div>
    <h2>Pending decisions (${pending.length})</h2>
    ${table(headers, pending, { cells, href: (c) => `#/candidates/${c.id}`, empty: "no candidates waiting" })}
    ${deferred.length ? `<h2>Deferred (${deferred.length})</h2>${table(headers, deferred, { cells, href: (c) => `#/candidates/${c.id}` })}` : ""}
  `;
  document.getElementById("start-run").onclick = async (e) => {
    e.target.disabled = true;
    try { const r = await post("/api/runs"); toast(`run ${short(r.id)} ${r.resumed ? "resumed" : "started"}`); location.hash = `#/runs/${r.id}`; }
    catch (err) { toast(err.message, true); e.target.disabled = false; }
  };
}
viewQueue.refresh = 5000;

// ---- candidate -------------------------------------------------------------------

async function viewCandidate(id) {
  const c = await api(`/api/candidates/${id}`);
  const r = c.review || {};
  const gate = r.gate;
  const llm = r.llm;
  const open = ["pending", "deferred", "publish_review", "failed"].includes(c.state);
  const publishFlags = (r.publish_flags || []).map((f) => `<li>${pill("high")} ${esc(f)}</li>`).join("");
  const diffP = r.from || r.to || c.aur_commit ? api(`/api/candidates/${id}/diff`).catch((e) => ({ error: e.message })) : Promise.resolve(null);
  const jobsP = api(`/api/jobs?candidate=${id}`).catch(() => []);
  const [diff, jobs] = await Promise.all([diffP, jobsP]);

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
    </div>`;
  }
  const commits = (r.commits || []).map((k) => `<tr><td class="mono">${esc(short(k.hash))}</td><td class="muted">${esc(k.date)}</td><td>${esc(k.author)}</td><td>${esc(k.subject)}</td></tr>`).join("");
  const scripts = (diff?.files || []).map((f) => `<h2>${esc(f.name)} <span class="pill">${esc(f.status)}</span></h2><pre class="diff">${f.diff ? renderDiff(f.diff) : esc(f.text || "")}</pre>`).join("");
  const classes = ["upstream-binary", "upstream-source", "vcs", "repackaged", "authored"];

  $main.innerHTML = `
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
    <div class="panel"><dl class="kv">
      <dt>version</dt><dd>${r.version_from ? `${esc(r.version_from)} → ` : ""}${dash(r.version_to)}</dd>
      <dt>commit</dt><dd class="mono">${r.from ? `${short(r.from)} → ` : ""}${short(c.aur_commit)}</dd>
      ${r.upstream_rev ? `<dt>upstream</dt><dd class="mono">${esc(r.upstream_rev)}</dd>` : ""}
      <dt>class</dt><dd>${dash(c.class)}</dd>
      <dt>gate</dt><dd>${pill(gate?.diff_class)} floor ${pill(c.floor_risk)} llm ${pill(c.llm_risk)}</dd>
      <dt>first seen on AUR</dt><dd>${agoEpoch(c.first_seen_last_modified)} <span class="muted">(quarantine clock)</span></dd>
      <dt>run</dt><dd><a href="#/runs/${esc(c.run_id)}">${short(c.run_id)}</a></dd>
      ${r.gate_error ? `<dt>gate error</dt><dd>${esc(r.gate_error)}</dd>` : ""}
      ${r.corroboration ? `<dt>upstream</dt><dd>${pill(r.corroboration.result === "confirmed" ? "low" : r.corroboration.result === "mismatch" ? "high" : "")}${esc(r.corroboration.result)} <span class="muted">${esc(r.corroboration.detail)}</span></dd>` : ""}
      ${r.note ? `<dt>note</dt><dd>${esc(r.note)}</dd>` : ""}
    </dl></div>
    ${publishFlags ? `<h2>Built artifacts raised flags</h2><div class="panel"><p class="muted">The build succeeded but the artifacts differ from what the stream serves in ways that matter on the installing system. Approve to rebuild and publish; reject to drop this commit.</p><ul class="findings">${publishFlags}</ul></div>` : ""}
    ${reasons ? `<h2>Reasons</h2><ul class="findings">${reasons}</ul>` : ""}
    ${findings ? `<h2>Lint findings</h2><ul class="findings">${findings}</ul>` : ""}
    ${llmHtml}
    ${diff?.error ? `<p class="muted">diff unavailable: ${esc(diff.error)}</p>` : ""}
    ${diff?.pkgbuild != null ? `<h2>PKGBUILD ${r.from ? "diff" : ""}</h2><pre class="diff">${r.from ? renderDiff(diff.pkgbuild) : esc(diff.pkgbuild)}</pre>` : ""}
    ${scripts}
    ${commits ? `<h2>Commits</h2><table><tbody>${commits}</tbody></table>` : ""}
    ${jobs.length ? `<h2>Builds</h2>${jobsTable(jobs)}` : ""}
  `;
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

function jobsTable(jobs) {
  return table(["job", "kind", "package", "stream", "state", "worker", "queued", "took", "result"], jobs, {
    href: (j) => `#/jobs/${j.id}`,
    cells: (j) => [
      `<span class="mono">${short(j.id)}</span>`, esc(j.job?.kind), dash(j.pkgbase), dash(j.stream), pill(j.state), dash(j.worker),
      `<span class="muted">${ago(j.queued_at)}</span>`, j.started_at ? dur(j.started_at, j.finished_at) : "-",
      `<span class="ellipsis">${j.error ? esc(j.error.split(" | ").slice(-1)[0]) : j.exit_code === 0 ? "ok" : dash(j.exit_code)}</span>`,
    ],
    empty: "no jobs",
  });
}

async function viewRuns() {
  const runs = await api("/api/runs");
  $main.innerHTML = `
    <div class="row between"><h1>Runs</h1><button class="primary" id="start-run">Start run</button></div>
    ${table(["run", "state", "step", "started", "took", "planned", "ok", "failed", "published"], runs, {
      href: (r) => `#/runs/${r.id}`,
      cells: (r) => [
        `<span class="mono">${short(r.id)}</span>`, pill(r.state), dash(r.step), `<span class="muted">${ago(r.started_at)}</span>`,
        dur(r.started_at, r.finished_at), dash(r.summary?.builds_planned), dash(r.summary?.builds_succeeded), dash(r.summary?.builds_failed), dash(r.summary?.published),
      ],
      empty: "no runs yet",
    })}`;
  document.getElementById("start-run").onclick = async (e) => {
    e.target.disabled = true;
    try { const r = await post("/api/runs"); location.hash = `#/runs/${r.id}`; }
    catch (err) { toast(err.message, true); e.target.disabled = false; }
  };
}
viewRuns.refresh = 5000;

async function viewRun(id) {
  const r = await api(`/api/runs/${id}`);
  const open = r.state === "running" || r.state === "needs_input";
  const summary = r.summary && Object.keys(r.summary).length ? `<pre>${esc(JSON.stringify(r.summary, null, 2))}</pre>` : "";
  const cands = table(["package", "kind", "state", "class", "diff", "floor", "llm", "summary"], r.candidates, {
    href: (c) => `#/candidates/${c.id}`,
    cells: (c) => [esc(c.pkgbase), esc(c.kind), pill(c.state), dash(c.class), pill(c.review?.gate?.diff_class), pill(c.floor_risk), pill(c.llm_risk), `<span class="ellipsis">${esc(candidateSummary(c))}</span>`],
    empty: "no candidates in this run",
  });
  const jobs = [...r.jobs].sort((a, b) => (b.queued_at > a.queued_at ? 1 : -1));
  $main.innerHTML = `
    <div class="row between">
      <h1>Run <span class="mono">${short(id)}</span> ${pill(r.state)} ${r.step ? `<span class="muted">step ${esc(r.step)}</span>` : ""}</h1>
      ${r.state === "needs_input" ? `<a class="btn" href="#/queue">Decide pending candidates</a>` : ""}
    </div>
    <div class="muted">started ${ago(r.started_at)} · ${open ? "running for" : "took"} ${dur(r.started_at, r.finished_at)}</div>
    ${summary}
    <h2>Candidates (${r.candidates.length})</h2>${cands}
    <h2>Jobs (${jobs.length})</h2>${jobsTable(jobs)}`;
}
viewRun.refresh = 4000;

async function viewJob(id) {
  const [j, log] = await Promise.all([api(`/api/jobs/${id}`), api(`/api/jobs/${id}/log`)]);
  const stages = (j.stages || []).map((s) => `<tr><td>${esc(s.stage)}</td><td class="muted">${ago(s.started_at)}</td><td>${s.finished_at ? dur(s.started_at, s.finished_at) : pill("running")}</td></tr>`).join("");
  const lines = log.map((l) => `<span class="${l.fd === "system" ? "sys" : l.fd === "stderr" ? "err" : ""}">${esc(l.line)}</span>`).join("\n");
  const live = ["queued", "assigned", "running"].includes(j.state);
  $main.innerHTML = `
    <h1>Job <span class="mono">${short(id)}</span> ${pill(j.state)} <span class="muted">${esc(j.job?.kind)} ${dash(j.pkgbase)} ${dash(j.stream)}</span></h1>
    <div class="panel"><dl class="kv">
      <dt>worker</dt><dd>${dash(j.worker)}</dd>
      <dt>queued</dt><dd>${ago(j.queued_at)}</dd>
      <dt>took</dt><dd>${j.started_at ? dur(j.started_at, j.finished_at) : "-"}</dd>
      <dt>exit</dt><dd>${dash(j.exit_code)}</dd>
      ${j.error ? `<dt>error</dt><dd>${esc(j.error)}</dd>` : ""}
      ${j.peaks?.memory_bytes ? `<dt>peak memory</dt><dd>${bytes(j.peaks.memory_bytes)}</dd>` : ""}
      ${j.job?.aur_commit ? `<dt>commit</dt><dd class="mono">${short(j.job.aur_commit)}</dd>` : ""}
      ${j.job?.allow_network ? `<dt>network</dt><dd>allowed during build (policy)</dd>` : ""}
    </dl></div>
    ${stages ? `<h2>Stages</h2><table><tbody>${stages}</tbody></table>` : ""}
    <h2>Log (${log.length} lines)${live ? ' <span class="pill running">live</span>' : ""}</h2>
    <pre class="log" id="log">${lines || '<span class="muted">no output yet</span>'}</pre>`;
  const pre = document.getElementById("log");
  if (live) pre.scrollTop = pre.scrollHeight;
}
viewJob.refresh = 3000;

// ---- packages --------------------------------------------------------------------

async function viewPackages() {
  const pkgs = await api("/api/packages");
  const q = (document.getElementById("filter")?.value || "").toLowerCase();
  const rows = pkgs.filter((p) => !q || p.pkgbase.includes(q) || (p.provenance_class || "").includes(q) || p.streams.some((s) => s.stream.includes(q)));
  const render = () => {
    const q2 = (document.getElementById("filter")?.value || "").toLowerCase();
    const rows2 = pkgs.filter((p) => !q2 || p.pkgbase.includes(q2) || (p.provenance_class || "").includes(q2) || p.streams.some((s) => s.stream.includes(q2)));
    document.getElementById("pkg-table").innerHTML = table(["package", "kind", "class", "streams", "AUR version", "maintainer", "votes", "baseline", "open", "policy"], rows2, {
      href: (p) => `#/packages/${p.pkgbase}`,
      cells: (p) => [
        esc(p.pkgbase), esc(p.kind), dash(p.provenance_class),
        p.streams.map((s) => `<span class="pill">${esc(s.stream.replace("manjaro-", ""))}${s.required && !s.wanted ? " (dep)" : ""}</span>`).join(" "),
        dash(p.snapshot?.version), dash(p.snapshot?.maintainer), dash(p.snapshot?.votes),
        p.baseline ? `<span class="mono">${short(p.baseline.aur_commit, 8)}</span> <span class="muted">${ago(p.baseline.published_at)}</span>` : '<span class="muted">none</span>',
        p.open_candidates.length ? `<a href="#/candidates/${p.open_candidates[0].id}">${pill(p.open_candidates[0].state)}</a>` : "",
        Object.keys(p.policy || {}).length ? `<code>${esc(JSON.stringify(p.policy))}</code>` : "",
      ],
      empty: "no packages match",
    });
  };
  $main.innerHTML = `
    <div class="row between">
      <h1>Packages <span class="muted">(${pkgs.length})</span></h1>
      <div class="row">
        <input id="filter" placeholder="filter" value="${esc(q)}">
        <input id="add-name" placeholder="pkgbase to add" size="18">
        <select id="add-stream"><option value="manjaro-stable">stable</option><option value="manjaro-unstable">unstable</option></select>
        <button id="add">Add</button>
      </div>
    </div>
    <div id="pkg-table"></div>`;
  render();
  document.getElementById("filter").oninput = render;
  document.getElementById("add").onclick = async () => {
    const pkgbase = document.getElementById("add-name").value.trim();
    const stream = document.getElementById("add-stream").value;
    if (!pkgbase) return;
    try { await post("/api/packages", { pkgbase, streams: [stream] }); toast(`${pkgbase} added to ${stream}`); location.hash = `#/packages/${pkgbase}`; }
    catch (e) { toast(e.message, true); }
  };
}

async function viewPackage(pkgbase) {
  const [p, h] = await Promise.all([api(`/api/packages/${pkgbase}`), api(`/api/packages/${pkgbase}/history`)]);
  const s = p.snapshot || {};
  const cands = table(["when", "kind", "state", "version", "diff", "floor", "llm", "decided by", "summary"], h.candidates, {
    href: (c) => `#/candidates/${c.id}`,
    cells: (c) => [`<span class="muted">${ago(c.first_seen_at)}</span>`, esc(c.kind), pill(c.state), dash(c.review?.version_to), pill(c.review?.gate?.diff_class), pill(c.floor_risk), pill(c.llm_risk), dash(c.decided_by), `<span class="ellipsis">${esc(candidateSummary(c))}</span>`],
    empty: "no candidates yet",
  });
  const pubs = table(["repo", "package", "version", "published", "retired", "size", "commit", "upstream"], h.publications, {
    cells: (x) => [esc(x.repo), esc(x.artifact.pkgname), esc(x.artifact.pkgver), `<span class="muted">${ago(x.published_at)}</span>`, x.retired_at ? `<span class="muted">${ago(x.retired_at)}</span>` : pill("live"), bytes(x.artifact.size), `<span class="mono">${short(x.artifact.aur_commit, 8)}</span>`, `<span class="mono">${short(x.artifact.upstream_rev, 12)}</span>`],
    empty: "nothing published yet",
  });
  $main.innerHTML = `
    <div class="row between">
      <h1>${esc(p.pkgbase)} <span class="muted">${esc(p.kind)} · ${dash(p.provenance_class)}</span></h1>
      <div class="row">
        <a class="btn" href="${p.source_url ? esc(p.source_url.split("#")[0]) : `https://aur.archlinux.org/packages/${esc(p.pkgbase)}`}" target="_blank" rel="noopener">${p.source_url ? "source repo" : "AUR page"}</a>
        <button class="danger" id="remove">Remove package</button>
      </div>
    </div>
    <div class="panel"><dl class="kv">
      <dt>streams</dt><dd>${p.streams.map((m) => `<span class="pill">${esc(m.stream)}${m.required && !m.wanted ? " (dependency)" : ""}</span>`).join(" ") || "-"}</dd>
      <dt>AUR version</dt><dd>${dash(s.version)} <span class="muted">modified ${agoEpoch(s.last_modified)}, fetched ${ago(s.fetched_at)}</span></dd>
      <dt>maintainer</dt><dd>${dash(s.maintainer)}${(s.comaintainers || []).length ? ` <span class="muted">+ ${esc(s.comaintainers.join(", "))}</span>` : ""}${s.out_of_date ? ` <span class="pill high">flagged out of date</span>` : ""}</dd>
      <dt>votes</dt><dd>${dash(s.votes)} <span class="muted">popularity ${s.popularity != null ? s.popularity.toFixed(2) : "-"}</span></dd>
      <dt>baseline</dt><dd>${p.baseline ? `<span class="mono">${short(p.baseline.aur_commit)}</span> published ${ago(p.baseline.published_at)}${p.baseline.maintainers?.length ? ` by ${esc(p.baseline.maintainers.join(", "))}` : ""}` : '<span class="muted">none yet</span>'}</dd>
      ${p.source_url ? `<dt>source</dt><dd class="mono">${esc(p.source_url)}</dd>` : ""}
      <dt>added</dt><dd>${ago(p.added_at)}</dd>
    </dl></div>
    <h2>Policy</h2>
    <div class="panel">
      <textarea class="mono" id="policy">${esc(JSON.stringify(p.policy || {}, null, 2))}</textarea>
      <div class="row" style="margin-top:8px">
        <button class="primary" id="save-policy">Save policy</button>
        <span class="muted">keys: pgp="skip", allow_network_build, build_once, provider_choices, extra_depends, quarantine_days, auto_metadata</span>
      </div>
    </div>
    <h2>Candidates</h2>${cands}
    <h2>Publications</h2>${pubs}`;
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
  const [workers, images, events, health] = await Promise.all([api("/api/workers"), api("/api/images"), api("/api/events?limit=60"), api("/api/health")]);
  $main.innerHTML = `
    <h1>System <span class="muted">repoman ${esc(health.version)}</span></h1>
    <h2>Workers</h2>
    ${table(["name", "arch", "slots", "memory", "status", "running", "capabilities", "last seen"], workers, {
      cells: (w) => [esc(w.name), dash(w.arch), dash(w.slots), bytes(w.mem_bytes), w.connected ? pill("connected") : pill("offline"), w.running.map((j) => `<a href="#/jobs/${esc(j)}" class="mono">${short(j)}</a>`).join(" ") || "-", (w.capabilities || []).join(" "), `<span class="muted">${ago(w.last_seen)}</span>`],
      empty: "no workers have connected",
    })}
    <h2>Base images</h2>
    ${table(["stream", "image", "size", "packages", "state", "built"], images, {
      cells: (i) => [esc(i.stream), `<span class="mono">${short(i.sha256, 12)}</span>`, bytes(i.size), dash(i.packages?.length), pill(i.state), `<span class="muted">${ago(i.created_at)}</span>`],
      empty: "no images yet",
    })}
    <h2>Recent events</h2>
    ${table(["when", "kind", "subject", "payload"], events, {
      cells: (e) => [`<span class="muted">${ago(e.ts)}</span>`, esc(e.kind), dash(e.subject), `<code class="ellipsis">${esc(JSON.stringify(e.payload))}</code>`],
      empty: "no events",
    })}
    <h2>Client configuration</h2>
    <div class="panel"><p>Add to <code>/etc/pacman.conf</code> above the distribution repos, with the stream for that machine:</p>
    <pre>[repoman]
SigLevel = Required DatabaseRequired
Server = ${esc(location.origin)}/repos/manjaro-stable/$arch</pre>
    <p>Import the signing key once: <code>curl -s ${esc(location.origin)}/repos/repoman.asc | sudo pacman-key --add - &amp;&amp; sudo pacman-key --lsign-key &lt;fingerprint&gt;</code></p></div>`;
}
viewSystem.refresh = 10000;

// ---- boot --------------------------------------------------------------------------

header();
route();
