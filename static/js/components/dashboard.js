// Dashboard component
import * as api from '../api.js';
import { navigate } from '../router.js';
import { showToast, formatStatus, formatBytes, escapeHtml } from './utils.js';

export async function init() {
    await Promise.all([
        loadStatus(),
        loadAttentionNeeded(),
        loadRecentBuilds(),
    ]);

    // Wire up header action buttons
    const checkAndBuildBtn = document.getElementById('check-and-build-btn');
    const checkUpdatesBtn = document.getElementById('check-updates-btn');

    if (checkAndBuildBtn) {
        checkAndBuildBtn.addEventListener('click', async () => {
            checkAndBuildBtn.disabled = true;
            checkAndBuildBtn.textContent = 'Running...';
            try {
                const result = await api.system.checkAndBuild();
                const msg = `Checked ${result.checked}, updated ${result.updated?.length ?? 0}, queued ${result.build_order?.length ?? 0} builds`;
                showToast(result.skipped?.length ? `${msg} (${result.skipped.length} skipped)` : msg, 'success');
            } catch (error) {
                showToast(`Error: ${error.message}`, 'error');
            } finally {
                checkAndBuildBtn.disabled = false;
                checkAndBuildBtn.textContent = 'Check & Build Updates';
                clearProgress();
            }
            await Promise.all([loadAttentionNeeded(), loadStatus()]);
        });
    }

    if (checkUpdatesBtn) {
        checkUpdatesBtn.addEventListener('click', async () => {
            checkUpdatesBtn.disabled = true;
            checkUpdatesBtn.textContent = 'Checking...';
            try {
                await api.system.checkUpdates();
                showToast('Update check complete', 'success');
            } catch (error) {
                showToast(`Error: ${error.message}`, 'error');
            } finally {
                checkUpdatesBtn.disabled = false;
                checkUpdatesBtn.textContent = 'Check Updates';
                clearProgress();
            }
            await Promise.all([loadAttentionNeeded(), loadStatus()]);
        });
    }

    // SSE event handler
    const handler = (e) => {
        const event = e.detail;
        if (event.type === 'build_completed' || event.type === 'package_status_changed') {
            loadAttentionNeeded();
            loadRecentBuilds();
        }
        if (event.type === 'check_updates_progress') {
            updateProgress(event.checked, event.total, event.current);
        }
        if (event.type === 'check_updates_complete') {
            clearProgress();
            loadAttentionNeeded();
            loadStatus();
        }
    };
    window.addEventListener('repoman:status', handler);

    // Return cleanup function
    return () => window.removeEventListener('repoman:status', handler);
}

function updateProgress(checked, total, current) {
    const el = document.getElementById('check-progress');
    if (!el) return;
    el.style.display = '';
    el.textContent = `Checking ${checked}/${total}${current ? `: ${current}` : ''}...`;
}

function clearProgress() {
    const el = document.getElementById('check-progress');
    if (!el) return;
    el.style.display = 'none';
    el.textContent = '';
}

async function loadStatus() {
    const container = document.getElementById('status-overview');
    if (!container) return;
    try {
        const [status, orphans] = await Promise.all([
            api.system.status(),
            api.packages.listOrphans()
        ]);

        const orphanCount = orphans?.length || 0;

        container.innerHTML = `
            <h2>Repository Status</h2>
            <div class="status-grid">
                <div class="stat-item">
                    <div class="stat-value">${status.packages?.total || 0}</div>
                    <div class="stat-label">Packages</div>
                </div>
                <div class="stat-item">
                    <div class="stat-value">${status.repo?.package_count || 0}</div>
                    <div class="stat-label">In Repo</div>
                </div>
                <div class="stat-item">
                    <div class="stat-value">${formatBytes(status.repo?.total_size || 0)}</div>
                    <div class="stat-label">Repo Size</div>
                </div>
                <div class="stat-item ${orphanCount > 0 ? 'warning' : ''}">
                    <div class="stat-value">${orphanCount}</div>
                    <div class="stat-label">Orphans</div>
                </div>
                <div class="stat-item">
                    <div class="stat-value">${status.podman ? 'OK' : 'N/A'}</div>
                    <div class="stat-label">Podman</div>
                </div>
                <div class="stat-item">
                    <div class="stat-value">${status.image ? 'OK' : 'Missing'}</div>
                    <div class="stat-label">Base Image</div>
                </div>
            </div>
        `;
    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load status: ${error.message}</p>`;
    }
}

async function loadAttentionNeeded() {
    const container = document.getElementById('attention-list');
    if (!container) return;
    try {
        const packages = await api.packages.list();

        const attention = packages.filter(pkg =>
            ['pending_review', 'update_available', 'failed'].includes(pkg.status)
        );

        if (attention.length === 0) {
            container.innerHTML = '<p class="empty-state">All packages are up to date!</p>';
            return;
        }

        const updatable = attention.filter(pkg => pkg.status === 'update_available' || pkg.status === 'pending_review');
        const headerHtml = updatable.length > 0 ? `
            <div class="attention-header">
                <button class="btn btn-primary" id="approve-build-all">Approve &amp; Build All (${updatable.length})</button>
            </div>
        ` : '';

        container.innerHTML = headerHtml + attention.map(pkg => `
            <div class="package-item" data-pkg="${escapeHtml(pkg.pkgbase)}">
                <span class="name">${escapeHtml(pkg.pkgbase)}</span>
                <span class="version">${escapeHtml(pkg.pkgver || '-')}</span>
                <span class="status-badge ${pkg.status}">${formatStatus(pkg.status)}</span>
                <div class="actions">
                    ${pkg.status === 'pending_review' || pkg.status === 'update_available' ?
                        `<button class="btn btn-sm btn-primary" data-action="approve" data-pkg="${escapeHtml(pkg.pkgbase)}">Approve</button>` : ''}
                    ${pkg.status === 'approved' || pkg.status === 'update_available' ?
                        `<button class="btn btn-sm btn-success" data-action="build" data-pkg="${escapeHtml(pkg.pkgbase)}">Build</button>` : ''}
                </div>
            </div>
        `).join('');

        // Click to navigate
        container.querySelectorAll('.package-item').forEach(item => {
            item.addEventListener('click', (e) => {
                if (e.target.tagName === 'BUTTON') return;
                navigate(`/packages/${item.dataset.pkg}`);
            });
        });

        const approveAllBtn = document.getElementById('approve-build-all');
        if (approveAllBtn) {
            approveAllBtn.addEventListener('click', async () => {
                approveAllBtn.disabled = true;
                approveAllBtn.textContent = 'Approving & building...';
                try {
                    const result = await api.packages.approveAndBuild();
                    const msg = `Approved ${result.approved.length}, queued ${result.build_order.length} builds`;
                    showToast(result.skipped.length ? `${msg} (${result.skipped.length} skipped)` : msg, 'success');
                } catch (error) {
                    showToast(`Error: ${error.message}`, 'error');
                }
                await loadAttentionNeeded();
            });
        }

        container.querySelectorAll('[data-action]').forEach(btn => {
            btn.addEventListener('click', async (e) => {
                e.stopPropagation();
                const action = btn.dataset.action;
                const pkg = btn.dataset.pkg;
                btn.disabled = true;

                try {
                    if (action === 'approve') {
                        await api.packages.approve(pkg);
                        showToast(`Approved ${pkg}`, 'success');
                    } else if (action === 'build') {
                        await api.packages.build(pkg);
                        showToast(`Build started for ${pkg}`, 'success');
                    }
                } catch (error) {
                    showToast(`Error: ${error.message}`, 'error');
                    btn.disabled = false;
                } finally {
                    await loadAttentionNeeded();
                }
            });
        });
    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load packages: ${error.message}</p>`;
    }
}

async function loadRecentBuilds() {
    const container = document.getElementById('recent-builds-list');
    if (!container) return;
    try {
        const packages = await api.packages.list();

        // Show packages that are currently building or recently failed — they have active build history
        const active = packages.filter(pkg => ['building', 'failed'].includes(pkg.status));

        if (active.length === 0) {
            container.innerHTML = '<p class="empty-state">No recent builds. See each package\'s detail page for build history.</p>';
            return;
        }

        container.innerHTML = active.map(pkg => `
            <div class="package-item" data-pkg="${escapeHtml(pkg.pkgbase)}">
                <span class="name">${escapeHtml(pkg.pkgbase)}</span>
                <span class="version">${escapeHtml(pkg.pkgver || '-')}</span>
                <span class="status-badge ${pkg.status}">${formatStatus(pkg.status)}</span>
                <div class="actions">
                    <a href="#/packages/${escapeHtml(pkg.pkgbase)}" class="btn btn-sm">View</a>
                </div>
            </div>
        `).join('');

        container.querySelectorAll('.package-item').forEach(item => {
            item.addEventListener('click', (e) => {
                if (e.target.tagName === 'A' || e.target.tagName === 'BUTTON') return;
                navigate(`/packages/${item.dataset.pkg}`);
            });
        });
    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load builds: ${error.message}</p>`;
    }
}
