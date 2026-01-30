// Package List component
import * as api from '../api.js';
import { navigate } from '../router.js';
import { showToast, formatRelativeTime } from './utils.js';

// Sort state
let currentSort = { column: 'name', direction: 'asc' };

export async function init() {
    const statusFilter = document.getElementById('status-filter');
    const reasonFilter = document.getElementById('reason-filter');
    const checkUpdatesBtn = document.getElementById('check-updates-btn');

    statusFilter.addEventListener('change', () => loadPackages());
    reasonFilter.addEventListener('change', () => loadPackages());

    checkUpdatesBtn.addEventListener('click', async () => {
        checkUpdatesBtn.disabled = true;
        checkUpdatesBtn.textContent = 'Checking...';
        try {
            const result = await api.system.checkUpdates();
            showToast(`Checked ${result.checked || 0} packages`, 'success');
            await loadPackages();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            checkUpdatesBtn.disabled = false;
            checkUpdatesBtn.textContent = 'Check Updates';
        }
    });

    await loadPackages();
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
    const container = document.getElementById('packages-table');
    const statusFilter = document.getElementById('status-filter').value;
    const reasonFilter = document.getElementById('reason-filter').value;

    try {
        let packages;

        // If orphans filter is selected, use the orphans endpoint
        if (reasonFilter === 'orphans') {
            packages = await api.packages.listOrphans();
        } else {
            packages = await api.packages.list();
        }

        // Apply status filter
        if (statusFilter) {
            packages = packages.filter(pkg => pkg.status === statusFilter);
        }

        // Apply reason filter (except orphans which is already filtered)
        if (reasonFilter && reasonFilter !== 'orphans') {
            packages = packages.filter(pkg => pkg.install_reason === reasonFilter);
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
                <div class="package-item" data-pkg="${pkg.pkgbase}">
                    <span class="name">${pkg.pkgbase}${formatUpstreamBadge(pkg)}</span>
                    <span class="version">${pkg.pkgver || '-'}</span>
                    <span class="reason-badge ${pkg.install_reason}">${pkg.install_reason}</span>
                    <span class="status-badge ${pkg.status}">${formatStatus(pkg.status)}</span>
                    <span class="updated">${formatRelativeTime(pkg.updated_at)}</span>
                    <div class="actions">
                        ${canApprove(pkg.status) ?
                            `<button class="btn btn-sm btn-primary" data-action="approve" data-pkg="${pkg.pkgbase}">Approve</button>` : ''}
                        ${canBuild(pkg.status) ?
                            `<button class="btn btn-sm btn-success" data-action="build" data-pkg="${pkg.pkgbase}">Build</button>` : ''}
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
        container.innerHTML = `<p class="error">Failed to load packages: ${error.message}</p>`;
    }
}

function canApprove(status) {
    return ['pending_review', 'update_available', 'failed'].includes(status);
}

function canBuild(status) {
    return ['approved', 'update_available'].includes(status);
}

function formatStatus(status) {
    return status.replace(/_/g, ' ');
}

function formatUpstreamBadge(pkg) {
    if (pkg.has_upstream === false && pkg.in_official_repos === true) {
        return '<span class="upstream-badge deprecated" title="No AUR upstream, but exists in official repos - consider removing">deprecated</span>';
    } else if (pkg.has_upstream === false) {
        return '<span class="upstream-badge local-only" title="No AUR upstream - local/custom package">local</span>';
    }
    return '';
}
