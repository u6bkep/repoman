// Dashboard component
import * as api from '../api.js';
import { navigate } from '../router.js';

export async function init() {
    await Promise.all([
        loadStatus(),
        loadAttentionNeeded(),
    ]);
}

async function loadStatus() {
    const container = document.getElementById('status-overview');
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
                    <div class="stat-value">${formatBytes(status.repo?.size || 0)}</div>
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
    try {
        const packages = await api.packages.list();

        const attention = packages.filter(pkg =>
            ['pending_review', 'update_available', 'failed'].includes(pkg.status)
        );

        if (attention.length === 0) {
            container.innerHTML = '<p class="empty-state">All packages are up to date!</p>';
            return;
        }

        container.innerHTML = attention.map(pkg => `
            <div class="package-item" data-pkg="${pkg.pkgbase}">
                <span class="name">${pkg.pkgbase}</span>
                <span class="version">${pkg.pkgver || '-'}</span>
                <span class="status-badge ${pkg.status}">${formatStatus(pkg.status)}</span>
                <div class="actions">
                    ${pkg.status === 'pending_review' || pkg.status === 'update_available' ?
                        `<button class="btn btn-sm btn-primary" data-action="approve" data-pkg="${pkg.pkgbase}">Approve</button>` : ''}
                    ${pkg.status === 'approved' || pkg.status === 'update_available' ?
                        `<button class="btn btn-sm btn-success" data-action="build" data-pkg="${pkg.pkgbase}">Build</button>` : ''}
                </div>
            </div>
        `).join('');

        // Add click handlers
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
                    await loadAttentionNeeded();
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

function formatStatus(status) {
    return status.replace(/_/g, ' ');
}

function formatBytes(bytes) {
    if (bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i]}`;
}

function showToast(message, type = 'info') {
    let container = document.querySelector('.toast-container');
    if (!container) {
        container = document.createElement('div');
        container.className = 'toast-container';
        document.body.appendChild(container);
    }

    const toast = document.createElement('div');
    toast.className = `toast ${type}`;
    toast.textContent = message;
    container.appendChild(toast);

    setTimeout(() => toast.remove(), 3000);
}
