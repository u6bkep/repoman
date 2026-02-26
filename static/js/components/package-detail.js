// Package Detail component
import * as api from '../api.js';
import { navigate } from '../router.js';
import { showToast, formatStatus, formatRelativeTime, escapeHtml } from './utils.js';

let currentPkg = null;
let currentReason = null;

export async function init(params) {
    currentPkg = null;
    currentReason = null;

    const { name } = params;
    currentPkg = name;

    document.getElementById('pkg-name').textContent = name;

    // Setup tabs
    document.querySelectorAll('.tab').forEach(tab => {
        tab.addEventListener('click', () => {
            document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
            document.querySelectorAll('.tab-pane').forEach(p => p.classList.remove('active'));
            tab.classList.add('active');
            document.getElementById(`tab-${tab.dataset.tab}`).classList.add('active');
        });
    });

    // Setup action buttons
    document.getElementById('btn-approve').addEventListener('click', handleApprove);
    document.getElementById('btn-build').addEventListener('click', handleBuild);
    document.getElementById('btn-approve-build-all').addEventListener('click', handleApproveAndBuildAll);
    document.getElementById('btn-check-update').addEventListener('click', handleCheckUpdate);
    document.getElementById('btn-remove').addEventListener('click', handleRemove);
    document.getElementById('btn-toggle-reason').addEventListener('click', handleToggleReason);

    // Load data
    await Promise.all([
        loadPackageInfo(name),
        loadPkgbuild(name),
        loadDiff(name),
        loadDeps(name),
        loadBuilds(name),
    ]);
}

async function loadPackageInfo(name) {
    try {
        const pkg = await api.packages.get(name);

        document.getElementById('pkg-status').className = `status-badge ${pkg.status}`;
        document.getElementById('pkg-status').textContent = formatStatus(pkg.status);
        document.getElementById('pkg-version').textContent = pkg.pkgver || '-';
        document.getElementById('pkg-commit').textContent = pkg.current_commit?.slice(0, 8) || '-';
        document.getElementById('pkg-updated').textContent = formatRelativeTime(pkg.updated_at);

        // Install reason
        currentReason = pkg.install_reason;
        const reasonEl = document.getElementById('pkg-reason');
        reasonEl.className = `reason-badge ${pkg.install_reason}`;
        reasonEl.textContent = pkg.install_reason;

        // Update toggle button text
        const toggleBtn = document.getElementById('btn-toggle-reason');
        if (pkg.install_reason === 'explicit') {
            toggleBtn.textContent = 'Mark as Dep';
        } else {
            toggleBtn.textContent = 'Mark Explicit';
        }

        // Upstream status warning
        let upstreamWarningEl = document.getElementById('upstream-warning');
        if (!upstreamWarningEl) {
            upstreamWarningEl = document.createElement('div');
            upstreamWarningEl.id = 'upstream-warning';
            const infoGrid = document.querySelector('.pkg-info-grid');
            if (infoGrid) {
                infoGrid.parentNode.insertBefore(upstreamWarningEl, infoGrid.nextSibling);
            }
        }

        if (pkg.has_upstream === false && pkg.in_official_repos === true) {
            upstreamWarningEl.className = 'upstream-warning';
            upstreamWarningEl.innerHTML = `
                <strong>⚠️ Deprecated:</strong> This package no longer exists in AUR but is available in the official Arch repositories.
                Consider removing it from your local repo and installing from official repos instead.
            `;
            upstreamWarningEl.style.display = '';
        } else if (pkg.has_upstream === false) {
            upstreamWarningEl.className = 'upstream-warning local-only';
            upstreamWarningEl.innerHTML = `
                <strong>ℹ️ Local Only:</strong> This package has no AUR upstream. It may have been removed from AUR or is a custom/local package.
            `;
            upstreamWarningEl.style.display = '';
        } else {
            upstreamWarningEl.style.display = 'none';
        }

        // Update button visibility
        const canApprove = ['pending_review', 'update_available', 'failed'].includes(pkg.status);
        const canBuild = ['approved', 'update_available'].includes(pkg.status);
        const showApproveAndBuildAll = pkg.status === 'pending_review';

        document.getElementById('btn-approve').style.display = canApprove ? '' : 'none';
        document.getElementById('btn-build').style.display = canBuild ? '' : 'none';
        document.getElementById('btn-approve-build-all').style.display = showApproveAndBuildAll ? '' : 'none';
    } catch (error) {
        showToast(`Error loading package: ${error.message}`, 'error');
    }
}

async function loadPkgbuild(name) {
    const container = document.getElementById('pkgbuild-content').querySelector('code');
    try {
        const result = await api.packages.pkgbuild(name);
        container.textContent = result.content || result;
    } catch (error) {
        container.textContent = `Error loading PKGBUILD: ${error.message}`;
    }
}

async function loadDiff(name) {
    const container = document.getElementById('diff-content').querySelector('code');
    try {
        const result = await api.packages.diff(name);
        const diff = result.diff || result;

        if (!diff || diff.trim() === '') {
            container.innerHTML = '<em>No changes since last approval</em>';
            return;
        }

        // Highlight diff lines
        container.innerHTML = diff.split('\n').map(line => {
            if (line.startsWith('+') && !line.startsWith('+++')) {
                return `<span class="line-add">${escapeHtml(line)}</span>`;
            } else if (line.startsWith('-') && !line.startsWith('---')) {
                return `<span class="line-del">${escapeHtml(line)}</span>`;
            } else if (line.startsWith('@@')) {
                return `<span class="line-hunk">${escapeHtml(line)}</span>`;
            }
            return escapeHtml(line);
        }).join('\n');
    } catch (error) {
        container.textContent = `Error loading diff: ${error.message}`;
    }
}

async function loadDeps(name) {
    const container = document.getElementById('deps-content');
    try {
        const deps = await api.packages.deps(name);

        // Check for untracked AUR deps across all dep types
        const allDeps = [
            ...(deps.depends || []),
            ...(deps.makedepends || []),
            ...(deps.checkdepends || []),
        ];
        const untrackedAurDeps = allDeps.filter(d => d.source === 'aur' && d.in_repo === false);
        showMissingDepsBanner(name, untrackedAurDeps);

        const renderDepList = (title, depList, showSource = true) => {
            if (!depList || depList.length === 0) return '';

            const items = depList.map(dep => {
                const sourceClass = dep.source || 'unknown';
                const trackedBadge = dep.in_repo ? '<span class="dep-badge tracked">tracked</span>' : '';
                const sourceBadge = showSource ? `<span class="dep-badge ${sourceClass}">${dep.source}</span>` : '';
                const constraint = dep.version_constraint ? `<span class="dep-constraint">${dep.version_constraint}</span>` : '';
                let nameHtml;
                if (dep.in_repo) {
                    nameHtml = `<a href="#/packages/${escapeHtml(dep.name)}" class="dep-name dep-link">${escapeHtml(dep.name)}</a>`;
                } else if (dep.source === 'official') {
                    nameHtml = `<a href="https://archlinux.org/packages/?q=${encodeURIComponent(dep.name)}" target="_blank" rel="noopener" class="dep-name dep-link dep-external">${escapeHtml(dep.name)}</a>`;
                } else {
                    nameHtml = `<span class="dep-name">${escapeHtml(dep.name)}</span>`;
                }

                return `
                    <div class="dep-item ${sourceClass}">
                        ${nameHtml}
                        ${constraint}
                        ${sourceBadge}
                        ${trackedBadge}
                    </div>
                `;
            }).join('');

            // Count AUR deps
            const aurCount = depList.filter(d => d.source === 'aur' || d.source === 'aur-tracked').length;
            const aurNote = aurCount > 0 ? `<span class="aur-count">(${aurCount} from AUR)</span>` : '';

            return `
                <div class="dep-section">
                    <h3>${title} ${aurNote}</h3>
                    <div class="dep-list">${items}</div>
                </div>
            `;
        };

        const renderOptdeps = (optdeps) => {
            if (!optdeps || optdeps.length === 0) return '';

            const items = optdeps.map(dep => `
                <div class="dep-item optional">
                    <span class="dep-name">${dep}</span>
                </div>
            `).join('');

            return `
                <div class="dep-section">
                    <h3>Optional Dependencies</h3>
                    <div class="dep-list">${items}</div>
                </div>
            `;
        };

        container.innerHTML = `
            ${renderDepList('Runtime Dependencies', deps.depends)}
            ${renderDepList('Build Dependencies', deps.makedepends)}
            ${renderDepList('Check Dependencies', deps.checkdepends)}
            ${renderOptdeps(deps.optdepends)}
        `;

        if (!deps.depends?.length && !deps.makedepends?.length && !deps.checkdepends?.length) {
            container.innerHTML = '<p class="empty-state">No dependencies</p>';
        }
    } catch (error) {
        container.innerHTML = `<p class="error">Error loading dependencies: ${error.message}</p>`;
    }
}

async function loadBuilds(name) {
    const container = document.getElementById('builds-list');
    try {
        const builds = await api.builds.list(name);

        if (!builds || builds.length === 0) {
            container.innerHTML = '<p class="empty-state">No builds yet</p>';
            return;
        }

        container.innerHTML = `
            <table class="builds-table">
                <thead>
                    <tr>
                        <th>ID</th>
                        <th>Commit</th>
                        <th>Status</th>
                        <th>Started</th>
                        <th>Actions</th>
                    </tr>
                </thead>
                <tbody>
                    ${builds.map(build => `
                        <tr>
                            <td class="mono">${build.id.slice(0, 8)}</td>
                            <td class="mono">${build.commit?.slice(0, 8) || '-'}</td>
                            <td><span class="status-badge ${build.status}">${build.status}</span></td>
                            <td>${formatRelativeTime(build.started_at)}</td>
                            <td>
                                <button class="btn btn-sm" data-action="view-log" data-id="${build.id}">Logs</button>
                            </td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
        `;

        container.querySelectorAll('[data-action="view-log"]').forEach(btn => {
            btn.addEventListener('click', () => navigate(`/builds/${btn.dataset.id}`));
        });
    } catch (error) {
        container.innerHTML = `<p class="error">Error loading builds: ${error.message}</p>`;
    }
}

async function handleApprove() {
    const btn = document.getElementById('btn-approve');
    btn.disabled = true;
    try {
        await api.packages.approve(currentPkg);
        showToast(`Approved ${currentPkg}`, 'success');
        await loadPackageInfo(currentPkg);
    } catch (error) {
        showToast(`Error: ${error.message}`, 'error');
    } finally {
        btn.disabled = false;
    }
}

async function handleBuild() {
    const btn = document.getElementById('btn-build');
    btn.disabled = true;
    try {
        const result = await api.packages.build(currentPkg);
        showToast(`Build started for ${currentPkg}`, 'success');
        if (result.id) {
            navigate(`/builds/${result.id}`);
        } else {
            await loadPackageInfo(currentPkg);
            await loadBuilds(currentPkg);
        }
    } catch (error) {
        showToast(`Error: ${error.message}`, 'error');
    } finally {
        btn.disabled = false;
    }
}

async function handleCheckUpdate() {
    const btn = document.getElementById('btn-check-update');
    btn.disabled = true;
    try {
        await api.packages.checkUpdate(currentPkg);
        showToast(`Checked ${currentPkg} for updates`, 'success');
        await loadPackageInfo(currentPkg);
        await loadDiff(currentPkg);
    } catch (error) {
        showToast(`Error: ${error.message}`, 'error');
    } finally {
        btn.disabled = false;
    }
}

async function handleRemove() {
    if (!confirm(`Are you sure you want to remove ${currentPkg}?`)) {
        return;
    }

    const btn = document.getElementById('btn-remove');
    btn.disabled = true;
    try {
        await api.packages.remove(currentPkg, true);
        showToast(`Removed ${currentPkg}`, 'success');
        navigate('/packages');
    } catch (error) {
        showToast(`Error: ${error.message}`, 'error');
        btn.disabled = false;
    }
}

async function handleToggleReason() {
    const btn = document.getElementById('btn-toggle-reason');
    btn.disabled = true;

    try {
        const newReason = currentReason === 'explicit' ? 'dependency' : 'explicit';
        await api.packages.mark(currentPkg, newReason);
        showToast(`Marked ${currentPkg} as ${newReason}`, 'success');
        await loadPackageInfo(currentPkg);
    } catch (error) {
        showToast(`Error: ${error.message}`, 'error');
    } finally {
        btn.disabled = false;
    }
}

async function handleApproveAndBuildAll() {
    const btn = document.getElementById('btn-approve-build-all');
    btn.disabled = true;
    btn.textContent = 'Starting builds...';
    try {
        await api.packages.approveAndBuild();
        showToast('Approved and queued builds for all pending packages', 'success');
        await loadPackageInfo(currentPkg);
    } catch (error) {
        showToast(`Error: ${error.message}`, 'error');
    } finally {
        btn.disabled = false;
        btn.textContent = 'Approve & Build All Pending';
    }
}

function showMissingDepsBanner(pkgbase, untrackedDeps) {
    // Remove any existing banner
    const existing = document.getElementById('missing-deps-banner');
    if (existing) existing.remove();

    if (!untrackedDeps || untrackedDeps.length === 0) return;

    const banner = document.createElement('div');
    banner.id = 'missing-deps-banner';
    banner.className = 'upstream-warning local-only';
    banner.innerHTML = `
        <strong>Info:</strong> This package has ${untrackedDeps.length} untracked AUR ${untrackedDeps.length === 1 ? 'dependency' : 'dependencies'}
        <span style="color: var(--text-muted); font-size: 0.85em;">(${untrackedDeps.map(d => escapeHtml(d.name)).join(', ')})</span>
        <div style="margin-top: 0.5rem;">
            <button id="btn-add-missing-deps" class="btn btn-primary btn-sm">Add Missing Dependencies</button>
        </div>
    `;

    // Insert above the tabs
    const tabs = document.querySelector('.tabs');
    if (tabs) {
        tabs.parentNode.insertBefore(banner, tabs);
    }

    banner.querySelector('#btn-add-missing-deps').addEventListener('click', async () => {
        const btn = banner.querySelector('#btn-add-missing-deps');
        btn.disabled = true;
        btn.textContent = 'Adding...';
        try {
            await api.packages.add(pkgbase, { withDeps: true });
            showToast('Added missing dependencies', 'success');
            // Refresh the page to show newly added deps
            await loadDeps(pkgbase);
            await loadPackageInfo(pkgbase);
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
            btn.disabled = false;
            btn.textContent = 'Add Missing Dependencies';
        }
    });
}
