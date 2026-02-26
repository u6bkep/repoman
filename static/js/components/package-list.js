// Package List component
import * as api from '../api.js';
import { navigate } from '../router.js';
import { showToast, formatStatus, formatRelativeTime, escapeHtml } from './utils.js';

// Sort state
let currentSort = { column: 'name', direction: 'asc' };
let currentController = null;

export async function init() {
    currentSort = { column: 'name', direction: 'asc' };
    currentController = null;
    const statusFilter = document.getElementById('status-filter');
    const reasonFilter = document.getElementById('reason-filter');
    const searchInput = document.getElementById('package-search');
    const checkUpdatesBtn = document.getElementById('check-updates-btn');

    statusFilter.addEventListener('change', () => loadPackages());
    reasonFilter.addEventListener('change', () => loadPackages());

    let searchTimeout;
    searchInput.addEventListener('input', () => {
        clearTimeout(searchTimeout);
        searchTimeout = setTimeout(() => loadPackages(), 200);
    });

    checkUpdatesBtn.addEventListener('click', async () => {
        checkUpdatesBtn.disabled = true;
        checkUpdatesBtn.textContent = 'Checking...';
        try {
            const result = await api.system.checkUpdates();
            showToast(`Checked ${result.checked} packages`, 'success');
            await loadPackages();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            checkUpdatesBtn.disabled = false;
            checkUpdatesBtn.textContent = 'Check Updates';
        }
    });

    const statusHandler = (e) => {
        const event = e.detail;
        if (event.type === 'package_status_changed') {
            const row = document.querySelector(`.package-item[data-pkg="${event.pkgbase}"]`);
            if (row) {
                const badge = row.querySelector('.status-badge');
                if (badge) {
                    badge.className = `status-badge ${event.new_status}`;
                    badge.textContent = formatStatus(event.new_status);
                }
                const actions = row.querySelector('.actions');
                if (actions) {
                    actions.innerHTML = '';
                    if (canApprove(event.new_status)) {
                        const approveBtn = document.createElement('button');
                        approveBtn.className = 'btn btn-sm btn-primary';
                        approveBtn.dataset.action = 'approve';
                        approveBtn.dataset.pkg = event.pkgbase;
                        approveBtn.textContent = 'Approve';
                        actions.appendChild(approveBtn);
                    }
                    if (canBuild(event.new_status)) {
                        const buildBtn = document.createElement('button');
                        buildBtn.className = 'btn btn-sm btn-success';
                        buildBtn.dataset.action = 'build';
                        buildBtn.dataset.pkg = event.pkgbase;
                        buildBtn.textContent = 'Build';
                        actions.appendChild(buildBtn);
                    }
                    actions.querySelectorAll('[data-action]').forEach(btn => {
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
                                await loadPackages();
                            } catch (error) {
                                showToast(`Error: ${error.message}`, 'error');
                                btn.disabled = false;
                            }
                        });
                    });
                }
            }
        }
        if (event.type === 'build_completed') {
            loadPackages();
        }
    };
    window.addEventListener('repoman:status', statusHandler);

    await loadPackages();

    return () => {
        window.removeEventListener('repoman:status', statusHandler);
    };
}

function sortPackages(packages) {
    const { column, direction } = currentSort;
    const modifier = direction === 'asc' ? 1 : -1;

    return [...packages].sort((a, b) => {
        let aVal, bVal;

        switch (column) {
            case 'name':
                aVal = a.pkgbase.toLowerCase();
                bVal = b.pkgbase.toLowerCase();
                break;
            case 'status':
                aVal = a.status;
                bVal = b.status;
                break;
            case 'updated':
                aVal = a.updated_at || '';
                bVal = b.updated_at || '';
                break;
            default:
                return 0;
        }

        if (aVal < bVal) return -1 * modifier;
        if (aVal > bVal) return 1 * modifier;
        return 0;
    });
}

function getSortIndicator(column) {
    if (currentSort.column !== column) return '';
    return currentSort.direction === 'asc' ? ' ▲' : ' ▼';
}

async function loadPackages() {
    if (currentController) currentController.abort();
    currentController = new AbortController();
    const signal = currentController.signal;

    const container = document.getElementById('packages-table');
    const statusFilter = document.getElementById('status-filter').value;
    const reasonFilter = document.getElementById('reason-filter').value;

    try {
        let packages;

        // If orphans filter is selected, use the orphans endpoint
        if (reasonFilter === 'orphans') {
            packages = await api.packages.listOrphans();
        } else {
            packages = await api.packages.list({ signal });
        }

        if (signal.aborted) return;

        // Apply status filter
        if (statusFilter) {
            packages = packages.filter(pkg => pkg.status === statusFilter);
        }

        // Apply reason filter (except orphans which is already filtered)
        if (reasonFilter && reasonFilter !== 'orphans') {
            packages = packages.filter(pkg => pkg.install_reason === reasonFilter);
        }

        // Apply search filter
        const searchQuery = document.getElementById('package-search').value.trim().toLowerCase();
        if (searchQuery) {
            packages = packages.filter(pkg => pkg.pkgbase.toLowerCase().includes(searchQuery));
        }

        if (packages.length === 0) {
            container.innerHTML = '<p class="empty-state">No packages found</p>';
            return;
        }

        // Sort packages
        packages = sortPackages(packages);

        // Render with sortable headers
        container.innerHTML = `
            <div class="package-header">
                <span class="name sortable" data-sort="name">Name${getSortIndicator('name')}</span>
                <span class="version">Version</span>
                <span class="reason">Reason</span>
                <span class="status sortable" data-sort="status">Status${getSortIndicator('status')}</span>
                <span class="updated sortable" data-sort="updated">Updated${getSortIndicator('updated')}</span>
                <span class="actions-header">Actions</span>
            </div>
            ${packages.map(pkg => `
                <div class="package-item" data-pkg="${escapeHtml(pkg.pkgbase)}">
                    <span class="name">${escapeHtml(pkg.pkgbase)}${formatUpstreamBadge(pkg)}</span>
                    <span class="version">${escapeHtml(pkg.pkgver || '-')}</span>
                    <span class="reason-badge ${pkg.install_reason}">${escapeHtml(pkg.install_reason)}</span>
                    <span class="status-badge ${pkg.status}">${formatStatus(pkg.status)}</span>
                    <span class="updated">${formatRelativeTime(pkg.updated_at)}</span>
                    <div class="actions">
                        ${canApprove(pkg.status) ?
                            `<button class="btn btn-sm btn-primary" data-action="approve" data-pkg="${escapeHtml(pkg.pkgbase)}">Approve</button>` : ''}
                        ${canBuild(pkg.status) ?
                            `<button class="btn btn-sm btn-success" data-action="build" data-pkg="${escapeHtml(pkg.pkgbase)}">Build</button>` : ''}
                    </div>
                </div>
            `).join('')}`;

        // Add click handlers for sortable headers
        container.querySelectorAll('.sortable').forEach(header => {
            header.addEventListener('click', () => {
                const column = header.dataset.sort;
                if (currentSort.column === column) {
                    currentSort.direction = currentSort.direction === 'asc' ? 'desc' : 'asc';
                } else {
                    currentSort.column = column;
                    currentSort.direction = 'asc';
                }
                loadPackages();
            });
        });

        // Add click handlers for package items
        container.querySelectorAll('.package-item').forEach(item => {
            item.addEventListener('click', (e) => {
                if (e.target.tagName === 'BUTTON') return;
                navigate(`/packages/${item.dataset.pkg}`);
            });
        });

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
                    await loadPackages();
                } catch (error) {
                    showToast(`Error: ${error.message}`, 'error');
                    btn.disabled = false;
                }
            });
        });
    } catch (error) {
        if (error.name === 'AbortError') return;
        container.innerHTML = `<p class="error">Failed to load packages: ${error.message}</p>`;
    }
}

function canApprove(status) {
    return ['pending_review', 'update_available', 'failed'].includes(status);
}

function canBuild(status) {
    return ['approved', 'update_available'].includes(status);
}

function formatUpstreamBadge(pkg) {
    if (pkg.has_upstream === false && pkg.in_official_repos === true) {
        return '<span class="upstream-badge deprecated" title="No AUR upstream, but exists in official repos - consider removing">deprecated</span>';
    } else if (pkg.has_upstream === false) {
        return '<span class="upstream-badge local-only" title="No AUR upstream - local/custom package">local</span>';
    }
    return '';
}
