// Add Package component
import * as api from '../api.js';
import { navigate } from '../router.js';
import { showToast, escapeHtml, formatStatus } from './utils.js';

export async function init() {
    const searchInput = document.getElementById('aur-search');
    const searchBtn = document.getElementById('search-btn');

    const doSearch = () => searchAur(searchInput.value.trim());

    searchBtn.addEventListener('click', doSearch);
    searchInput.addEventListener('keypress', (e) => {
        if (e.key === 'Enter') doSearch();
    });

    // Focus the input
    searchInput.focus();
}

async function searchAur(query) {
    const container = document.getElementById('search-results');

    if (!query) {
        container.innerHTML = '<p class="hint">Enter a package name to search the AUR</p>';
        return;
    }

    container.innerHTML = '<div class="status-loading">Searching...</div>';

    try {
        const results = await api.aur.search(query);

        if (!results || results.length === 0) {
            container.innerHTML = '<p class="empty-state">No packages found</p>';
            return;
        }

        container.innerHTML = results.map(pkg => `
            <div class="search-result-item">
                <div class="info">
                    <h4>${escapeHtml(pkg.Name)}</h4>
                    <p>${escapeHtml(pkg.Description || 'No description')}</p>
                    <div class="meta">
                        <span>v${escapeHtml(pkg.Version)}</span>
                        <span>${pkg.NumVotes} votes</span>
                        ${pkg.OutOfDate ? '<span style="color: var(--warning)">Out of date</span>' : ''}
                        ${pkg.tracked ? '<span style="color: var(--success)">Already tracked</span>' : ''}
                    </div>
                </div>
                <div>
                    ${pkg.tracked ?
                        `<button class="btn" data-action="view" data-pkg="${escapeHtml(pkg.PackageBase)}">View</button>` :
                        `<button class="btn btn-primary" data-action="add" data-pkg="${escapeHtml(pkg.Name)}">Add</button>`
                    }
                </div>
            </div>
        `).join('');

        // Add click handlers
        container.querySelectorAll('[data-action="add"]').forEach(btn => {
            btn.addEventListener('click', async () => {
                btn.disabled = true;
                btn.textContent = 'Adding...';
                try {
                    const withDeps = document.getElementById('add-with-deps')?.checked || false;
                    const result = await api.packages.add(btn.dataset.pkg, { withDeps });
                    const pkgbase = result.pkgbase || btn.dataset.pkg;

                    if (withDeps) {
                        // Show post-add summary panel instead of navigating away
                        showAddSummary(container, pkgbase, result);
                    } else if (result.missing_deps && result.missing_deps.filter(d => d.in_aur).length > 0) {
                        // Added without deps but there are missing AUR deps — show warning panel
                        showMissingDepsWarning(container, pkgbase, result);
                    } else {
                        // No deps scenario, navigate directly
                        showToast(`Added ${pkgbase}`, 'success');
                        navigate(`/packages/${pkgbase}`);
                    }
                } catch (error) {
                    showToast(`Error: ${error.message}`, 'error');
                    btn.disabled = false;
                    btn.textContent = 'Add';
                }
            });
        });

        container.querySelectorAll('[data-action="view"]').forEach(btn => {
            btn.addEventListener('click', () => {
                navigate(`/packages/${btn.dataset.pkg}`);
            });
        });
    } catch (error) {
        container.innerHTML = `<p class="error">Search failed: ${error.message}</p>`;
    }
}

function showAddSummary(container, pkgbase, result) {
    const addedDeps = result.added_deps || [];
    const totalAdded = addedDeps.length;

    const allPackages = [
        { pkgbase, pkgver: result.pkgver || '-', status: result.status || 'pending_review', isDep: false },
        ...addedDeps.map(d => ({ pkgbase: d.pkgbase, pkgver: d.pkgver || '-', status: d.status || 'pending_review', isDep: true })),
    ];

    const tableRows = allPackages.map(pkg => `
        <tr>
            <td class="mono">${escapeHtml(pkg.pkgbase)}${pkg.isDep ? ' <span class="dep-badge aur">dep</span>' : ''}</td>
            <td class="mono">${escapeHtml(pkg.pkgver)}</td>
            <td><span class="status-badge ${escapeHtml(pkg.status)}">${formatStatus(pkg.status)}</span></td>
        </tr>
    `).join('');

    const summary = totalAdded > 0
        ? `Added ${escapeHtml(pkgbase)} and ${totalAdded} ${totalAdded === 1 ? 'dependency' : 'dependencies'}`
        : `Added ${escapeHtml(pkgbase)}`;

    container.innerHTML = `
        <div class="add-summary-panel">
            <div class="add-summary-header">
                <span class="add-summary-icon">&#10003;</span>
                <strong>${summary}</strong>
            </div>
            <table class="data-table add-summary-table">
                <thead>
                    <tr>
                        <th>Package</th>
                        <th>Version</th>
                        <th>Status</th>
                    </tr>
                </thead>
                <tbody>
                    ${tableRows}
                </tbody>
            </table>
            <div class="add-summary-actions">
                <button id="btn-approve-build-all" class="btn btn-primary">Approve &amp; Build All</button>
                <button id="btn-view-pkg" class="btn">View ${escapeHtml(pkgbase)}</button>
            </div>
        </div>
    `;

    container.querySelector('#btn-approve-build-all').addEventListener('click', async () => {
        const btn = container.querySelector('#btn-approve-build-all');
        btn.disabled = true;
        btn.textContent = 'Starting builds...';
        try {
            await api.packages.approveAndBuild();
            showToast('Approved and queued builds for all pending packages', 'success');
            navigate('/');
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
            btn.disabled = false;
            btn.textContent = 'Approve & Build All';
        }
    });

    container.querySelector('#btn-view-pkg').addEventListener('click', () => {
        navigate(`/packages/${pkgbase}`);
    });
}

function showMissingDepsWarning(container, pkgbase, result) {
    const missingAurDeps = (result.missing_deps || []).filter(d => d.in_aur).map(d => d.name);

    showToast(`Added ${pkgbase}`, 'success');

    // Prepend warning panel before search results
    const warning = document.createElement('div');
    warning.className = 'upstream-warning local-only missing-deps-warning';
    warning.innerHTML = `
        <strong>Warning:</strong> Missing AUR dependencies: ${missingAurDeps.map(n => `<code>${escapeHtml(n)}</code>`).join(', ')}
        <div style="margin-top: 0.5rem;">
            <button id="btn-add-missing-deps" class="btn btn-primary btn-sm">Add Missing Dependencies</button>
            <button id="btn-view-pkg-anyway" class="btn btn-sm" style="margin-left: 0.5rem;">View ${escapeHtml(pkgbase)}</button>
        </div>
    `;

    container.insertBefore(warning, container.firstChild);

    warning.querySelector('#btn-add-missing-deps').addEventListener('click', async () => {
        const btn = warning.querySelector('#btn-add-missing-deps');
        btn.disabled = true;
        btn.textContent = 'Adding...';
        try {
            const result2 = await api.packages.add(pkgbase, { withDeps: true });
            showAddSummary(container, pkgbase, result2);
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
            btn.disabled = false;
            btn.textContent = 'Add Missing Dependencies';
        }
    });

    warning.querySelector('#btn-view-pkg-anyway').addEventListener('click', () => {
        navigate(`/packages/${pkgbase}`);
    });
}
