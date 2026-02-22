// Settings component
import * as api from '../api.js';
import { showToast, formatBytes, escapeHtml } from './utils.js';

export async function init() {
    await Promise.all([
        loadSystemStatus(),
        loadContainers(),
        loadImages(),
        loadConfig(),
    ]);
    setupActionButtons();
    setupConfigTabs();
}

async function loadSystemStatus() {
    const container = document.getElementById('system-status');

    try {
        const status = await api.system.status();

        const orphanCount = status.orphaned_data
            ? (status.orphaned_data.clones.length + status.orphaned_data.sources.length +
               status.orphaned_data.pkgdests.length + status.orphaned_data.logs.length)
            : 0;

        container.innerHTML = `
            <div class="info-card">
                <h3>Podman</h3>
                <div>${status.podman ? 'Available' : 'Not available'}</div>
            </div>
            <div class="info-card">
                <h3>Base Image</h3>
                <div>${status.image ? 'Built' : 'Not built'}</div>
            </div>
            <div class="info-card">
                <h3>Repository</h3>
                <div>${status.repo?.package_count || 0} packages</div>
                <div>${formatBytes(status.repo?.size || 0)}</div>
            </div>
            <div class="info-card">
                <h3>Pacman Cache</h3>
                <div>${status.pacman_cache?.package_count || 0} packages</div>
                <div>${formatBytes(status.pacman_cache?.total_size || 0)}</div>
            </div>
            <div class="info-card">
                <h3>Source Cache</h3>
                <div>${status.source_cache?.package_count || 0} packages</div>
                <div>${formatBytes(status.source_cache?.total_size || 0)}</div>
            </div>
            <div class="info-card ${orphanCount > 0 ? 'warning' : ''}">
                <h3>Orphaned Data</h3>
                <div>${orphanCount} items</div>
                <div>${formatBytes(status.orphaned_data?.total_size || 0)}</div>
            </div>
            <div class="info-card">
                <h3>Packages Tracked</h3>
                <div>${status.packages?.total || 0} total</div>
            </div>
        `;

    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load status: ${error.message}</p>`;
    }
}

async function loadConfig() {
    const container = document.getElementById('config-display');
    if (!container) return;

    try {
        // Load both current config and template
        const [status, template] = await Promise.all([
            api.system.status(),
            api.system.configTemplate(),
        ]);

        // Create tabbed interface
        container.innerHTML = `
            <div class="config-tabs">
                <button class="config-tab active" data-tab="current">Current Config</button>
                <button class="config-tab" data-tab="template">Config Template</button>
            </div>
            <div class="config-content">
                <div id="config-current" class="config-pane active">
                    <pre>${formatConfigToml(status.config)}</pre>
                </div>
                <div id="config-template" class="config-pane">
                    <pre>${escapeHtml(template)}</pre>
                </div>
            </div>
        `;
    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load config: ${error.message}</p>`;
    }
}

function setupConfigTabs() {
    const container = document.getElementById('config-display');
    if (!container) return;

    container.addEventListener('click', (e) => {
        if (e.target.classList.contains('config-tab')) {
            const tabName = e.target.dataset.tab;

            // Update active tab button
            container.querySelectorAll('.config-tab').forEach(t => t.classList.remove('active'));
            e.target.classList.add('active');

            // Show/hide panes
            container.querySelectorAll('.config-pane').forEach(p => p.classList.remove('active'));
            const pane = container.querySelector(`#config-${tabName}`);
            if (pane) pane.classList.add('active');
        }
    });
}

function formatConfigToml(config) {
    // Format config object as TOML-like output
    const lines = [];

    if (config.repo) {
        lines.push('[repo]');
        lines.push(`path = "${config.repo.path}"`);
        lines.push(`name = "${config.repo.name}"`);
        lines.push(`keep_versions = ${config.repo.keep_versions}`);
        lines.push('');
    }

    if (config.cache) {
        lines.push('[cache]');
        if (config.cache.aur_clones) lines.push(`aur_clones = "${config.cache.aur_clones}"`);
        if (config.cache.pacman_cache) lines.push(`pacman_cache = "${config.cache.pacman_cache}"`);
        if (config.cache.logs) lines.push(`logs = "${config.cache.logs}"`);
        if (config.cache.sources) lines.push(`sources = "${config.cache.sources}"`);
        lines.push('');
    }

    if (config.container) {
        lines.push('[container]');
        lines.push(`image = "${config.container.image}"`);
        lines.push(`tmpfs_build = ${config.container.tmpfs_build}`);
        if (config.container.tmpfs_size) lines.push(`tmpfs_size = "${config.container.tmpfs_size}"`);
        lines.push('');
    }

    if (config.server) {
        lines.push('[server]');
        lines.push(`listen = "${config.server.listen}"`);
    }

    return escapeHtml(lines.join('\n'));
}

async function loadContainers() {
    const container = document.getElementById('containers-list');
    if (!container) return;

    try {
        const containers = await api.system.listContainers();

        if (containers.length === 0) {
            container.innerHTML = '<p class="muted">No repoman containers found</p>';
            return;
        }

        const stoppedCount = containers.filter(c => !c.running).length;
        container.innerHTML = `
            <table class="data-table">
                <thead>
                    <tr>
                        <th>Name</th>
                        <th>Status</th>
                        <th>Created</th>
                    </tr>
                </thead>
                <tbody>
                    ${containers.map(c => `
                        <tr>
                            <td><code>${c.name}</code></td>
                            <td><span class="status-badge ${c.running ? 'building' : 'failed'}">${c.running ? 'Running' : 'Stopped'}</span></td>
                            <td>${c.created}</td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
            <p class="muted">${containers.length} container(s), ${stoppedCount} stopped</p>
        `;
    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load containers: ${error.message}</p>`;
    }
}

async function loadImages() {
    const container = document.getElementById('images-list');
    if (!container) return;

    try {
        const images = await api.system.listImages();

        if (images.length === 0) {
            container.innerHTML = '<p class="muted">No repoman images found</p>';
            return;
        }

        container.innerHTML = `
            <table class="data-table">
                <thead>
                    <tr>
                        <th>Tag</th>
                        <th>ID</th>
                        <th>Created</th>
                        <th>Size</th>
                    </tr>
                </thead>
                <tbody>
                    ${images.map(img => `
                        <tr>
                            <td><code>${img.tag}</code></td>
                            <td><code>${img.id.substring(0, 12)}</code></td>
                            <td>${img.created}</td>
                            <td>${img.size}</td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
            <p class="muted">${images.length} image(s)</p>
        `;
    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load images: ${error.message}</p>`;
    }
}

function setupActionButtons() {
    document.getElementById('btn-rebuild-image').addEventListener('click', async (e) => {
        const btn = e.target;
        if (!confirm('Rebuild the base container image? This may take several minutes.')) {
            return;
        }
        btn.disabled = true;
        btn.textContent = 'Rebuilding...';
        try {
            await api.system.rebuildImage();
            showToast('Base image rebuilt successfully', 'success');
            await Promise.all([loadSystemStatus(), loadImages()]);
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            btn.disabled = false;
            btn.textContent = 'Rebuild Base Image';
        }
    });

    document.getElementById('btn-clean-cache').addEventListener('click', async (e) => {
        const btn = e.target;
        if (!confirm('Clean pacman cache? This keeps only the 2 most recent versions of each package.')) {
            return;
        }
        btn.disabled = true;
        btn.textContent = 'Cleaning...';
        try {
            const result = await api.system.cleanCache();
            if (result.packages_removed > 0) {
                showToast(`Removed ${result.packages_removed} old package(s), freed ${formatBytes(result.size_freed)}`, 'success');
            } else {
                showToast('Pacman cache already clean', 'success');
            }
            await loadSystemStatus();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            btn.disabled = false;
            btn.textContent = 'Clean Pacman Cache';
        }
    });

    document.getElementById('btn-clean-sources').addEventListener('click', async (e) => {
        const btn = e.target;
        if (!confirm('Clean all cached sources? This will require re-downloading sources on next build.')) {
            return;
        }
        btn.disabled = true;
        btn.textContent = 'Cleaning...';
        try {
            const result = await api.system.cleanSources();
            showToast(`Cleaned ${result.cleaned_count} package(s), freed ${formatBytes(result.cleaned_size)}`, 'success');
            await loadSystemStatus();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            btn.disabled = false;
            btn.textContent = 'Clean Source Cache';
        }
    });

    document.getElementById('btn-clean-orphans').addEventListener('click', async (e) => {
        const btn = e.target;
        if (!confirm('Clean all orphaned data (clones, sources, build artifacts for removed packages)?')) {
            return;
        }
        btn.disabled = true;
        btn.textContent = 'Cleaning...';
        try {
            const result = await api.system.cleanOrphans();
            const total = result.clones_removed + result.sources_removed +
                         result.pkgdests_removed + result.logs_removed;
            if (total > 0) {
                showToast(`Cleaned ${total} orphaned item(s), freed ${formatBytes(result.total_size_freed)}`, 'success');
            } else {
                showToast('No orphaned data found', 'success');
            }
            await loadSystemStatus();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            btn.disabled = false;
            btn.textContent = 'Clean Orphaned Data';
        }
    });

    document.getElementById('btn-prune-containers')?.addEventListener('click', async (e) => {
        const btn = e.target;
        if (!confirm('Remove all stopped repoman build containers?')) {
            return;
        }
        btn.disabled = true;
        btn.textContent = 'Pruning...';
        try {
            const result = await api.system.pruneContainers();
            if (result.removed.length > 0) {
                showToast(`Removed ${result.removed.length} container(s)`, 'success');
            } else {
                showToast('No stopped containers to remove', 'success');
            }
            await loadContainers();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            btn.disabled = false;
            btn.textContent = 'Prune Stopped Containers';
        }
    });

    document.getElementById('btn-prune-images')?.addEventListener('click', async (e) => {
        const btn = e.target;
        if (!confirm('Remove old repoman builder images? (Keeps the 3 most recent)')) {
            return;
        }
        btn.disabled = true;
        btn.textContent = 'Pruning...';
        try {
            const result = await api.system.pruneImages(3);
            if (result.removed.length > 0) {
                showToast(`Removed ${result.removed.length} old image(s)`, 'success');
            } else {
                showToast('No old images to remove', 'success');
            }
            await loadImages();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            btn.disabled = false;
            btn.textContent = 'Prune Old Images';
        }
    });

    document.getElementById('btn-rebuild-db').addEventListener('click', async (e) => {
        const btn = e.target;
        if (!confirm('Rebuild the repository database from package files?')) {
            return;
        }
        btn.disabled = true;
        try {
            await api.system.rebuildDb();
            showToast('Repository database rebuilt', 'success');
            await loadSystemStatus();
        } catch (error) {
            showToast(`Error: ${error.message}`, 'error');
        } finally {
            btn.disabled = false;
        }
    });
}
