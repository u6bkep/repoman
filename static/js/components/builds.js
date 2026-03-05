// Builds page component — global build list
import * as api from '../api.js';
import { navigate } from '../router.js';
import { showToast, formatStatus, formatStage, formatRelativeTime, formatDuration, escapeHtml } from './utils.js';

let currentFilter = null;
let sseHandler = null;

export async function init() {
    setupFilterTabs();
    await loadBuilds();

    sseHandler = (e) => {
        const event = e.detail;
        if (event.type === 'build_started' || event.type === 'build_completed' || event.type === 'build_stage_changed') {
            loadBuilds();
        }
    };
    window.addEventListener('repoman:status', sseHandler);

    return () => {
        if (sseHandler) {
            window.removeEventListener('repoman:status', sseHandler);
            sseHandler = null;
        }
    };
}

function setupFilterTabs() {
    const tabs = document.getElementById('build-filter-tabs');
    if (!tabs) return;

    tabs.addEventListener('click', (e) => {
        const btn = e.target.closest('[data-filter]');
        if (!btn) return;

        tabs.querySelectorAll('[data-filter]').forEach(b => b.classList.remove('active'));
        btn.classList.add('active');

        const filter = btn.dataset.filter || null;
        currentFilter = filter === 'all' ? null : filter;
        loadBuilds();
    });
}

async function loadBuilds() {
    const container = document.getElementById('builds-table');
    if (!container) return;

    try {
        const params = { limit: 50 };
        if (currentFilter) params.status = currentFilter;

        const builds = await api.builds.listAll(params);

        if (builds.length === 0) {
            container.innerHTML = `<p class="empty-state">${currentFilter ? `No ${currentFilter} builds.` : 'No builds yet.'}</p>`;
            return;
        }

        container.innerHTML = `
            <table class="data-table">
                <thead>
                    <tr>
                        <th>Package</th>
                        <th>Status</th>
                        <th>Stage</th>
                        <th>Started</th>
                        <th>Duration</th>
                        <th>Actions</th>
                    </tr>
                </thead>
                <tbody>
                    ${builds.map(b => buildRow(b)).join('')}
                </tbody>
            </table>
        `;

        // Wire up cancel buttons
        container.querySelectorAll('[data-cancel]').forEach(btn => {
            btn.addEventListener('click', async (e) => {
                e.stopPropagation();
                const id = btn.dataset.cancel;
                btn.disabled = true;
                btn.textContent = 'Cancelling...';
                try {
                    await api.builds.cancel(id);
                    showToast('Build cancelled', 'success');
                    await loadBuilds();
                } catch (error) {
                    showToast(`Error: ${error.message}`, 'error');
                    btn.disabled = false;
                    btn.textContent = 'Cancel';
                }
            });
        });

        // Click row to navigate to build log
        container.querySelectorAll('tbody tr[data-build-id]').forEach(row => {
            row.addEventListener('click', (e) => {
                if (e.target.tagName === 'BUTTON' || e.target.tagName === 'A') return;
                navigate(`/builds/${row.dataset.buildId}`);
            });
        });
    } catch (error) {
        container.innerHTML = `<p class="error">Failed to load builds: ${escapeHtml(error.message)}</p>`;
    }
}

function buildRow(build) {
    const isActive = build.status === 'running' || build.status === 'pending';
    const duration = formatDuration(build.started_at, build.finished_at);
    const stage = build.current_stage ? formatStage(build.current_stage) : '';

    return `
        <tr data-build-id="${escapeHtml(build.id)}" class="clickable">
            <td><a href="#/packages/${escapeHtml(build.pkgbase)}">${escapeHtml(build.pkgbase)}</a></td>
            <td><span class="status-badge ${build.status}">${formatStatus(build.status)}</span></td>
            <td>${isActive && stage ? `<span class="stage-badge ${escapeHtml(build.current_stage)}">${escapeHtml(stage)}</span>` : ''}</td>
            <td>${formatRelativeTime(build.started_at)}</td>
            <td>${duration}</td>
            <td>
                <a href="#/builds/${escapeHtml(build.id)}" class="btn btn-sm">Logs</a>
                ${isActive ? `<button class="btn btn-sm btn-danger" data-cancel="${escapeHtml(build.id)}">Cancel</button>` : ''}
            </td>
        </tr>
    `;
}

