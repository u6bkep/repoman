// Add Package component
import * as api from '../api.js';
import { navigate } from '../router.js';
import { showToast, escapeHtml } from './utils.js';

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

                    // Show summary of what was added
                    if (result.added_deps && result.added_deps.length > 0) {
                        const depNames = result.added_deps.map(d => d.pkgbase).join(', ');
                        showToast(`Added ${btn.dataset.pkg} and ${result.added_deps.length} dependencies (${depNames})`, 'success');
                    } else {
                        showToast(`Added ${btn.dataset.pkg}`, 'success');
                    }

                    // Show warning if there are still missing deps
                    if (result.missing_deps && result.missing_deps.length > 0) {
                        const missingNames = result.missing_deps.filter(d => d.in_aur).map(d => d.name).join(', ');
                        if (missingNames) {
                            showToast(`Missing AUR dependencies: ${missingNames}`, 'warning');
                        }
                    }

                    navigate(`/packages/${result.pkgbase || btn.dataset.pkg}`);
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
