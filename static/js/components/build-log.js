// Build Log component
import * as api from '../api.js';
import { formatStatus, formatStage, formatRelativeTime, formatDuration, escapeHtml } from './utils.js';

let cleanup = null;
let currentBuildId = null;
let searchIndex = -1;
let searchMatches = [];
let isMounted = false;
let autoScroll = true;
let durationInterval = null;
let lastActiveStage = null;
let currentBuildData = null;

export async function init(params) {
    cleanup = null;
    currentBuildId = null;
    searchIndex = -1;
    searchMatches = [];
    isMounted = true;
    autoScroll = true;
    lastActiveStage = null;
    currentBuildData = null;
    clearDurationInterval();

    const { id } = params;
    currentBuildId = id;

    setupAutoScroll();
    setupLogControls(id);
    setupCancelButton(id);
    await loadBuildInfo(id);
    await loadLogs(id);

    // Return cleanup function
    return () => {
        isMounted = false;
        clearDurationInterval();
        if (cleanup) {
            cleanup();
            cleanup = null;
        }
    };
}

function clearDurationInterval() {
    if (durationInterval) {
        clearInterval(durationInterval);
        durationInterval = null;
    }
}

function setupLogControls(buildId) {
    const logContainer = document.querySelector('.log-container');

    // Add controls bar if it doesn't exist
    let controls = document.querySelector('.log-controls');
    if (!controls) {
        controls = document.createElement('div');
        controls.className = 'log-controls';
        controls.innerHTML = `
            <div class="search-box">
                <input type="text" id="log-search" placeholder="Search logs...">
                <span id="search-count"></span>
                <button id="search-prev" class="btn btn-sm" disabled>▲</button>
                <button id="search-next" class="btn btn-sm" disabled>▼</button>
            </div>
            <button id="download-log" class="btn btn-sm">Download Log</button>
        `;
        logContainer.parentNode.insertBefore(controls, logContainer);
    }

    // Search functionality
    const searchInput = document.getElementById('log-search');
    const searchCount = document.getElementById('search-count');
    const searchPrev = document.getElementById('search-prev');
    const searchNext = document.getElementById('search-next');

    searchInput.addEventListener('input', () => {
        performSearch(searchInput.value);
    });

    searchInput.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
            e.preventDefault();
            if (e.shiftKey) {
                navigateSearch(-1);
            } else {
                navigateSearch(1);
            }
        }
    });

    searchPrev.addEventListener('click', () => navigateSearch(-1));
    searchNext.addEventListener('click', () => navigateSearch(1));

    // Download functionality
    document.getElementById('download-log').addEventListener('click', async () => {
        try {
            const logs = await api.builds.logs(buildId);
            const blob = new Blob([logs], { type: 'text/plain' });
            const url = URL.createObjectURL(blob);
            const a = document.createElement('a');
            a.href = url;
            a.download = `build-${buildId}.log`;
            a.click();
            URL.revokeObjectURL(url);
        } catch (error) {
            console.error('Failed to download log:', error);
        }
    });
}

function performSearch(query) {
    const container = document.getElementById('log-content');
    const searchCount = document.getElementById('search-count');
    const searchPrev = document.getElementById('search-prev');
    const searchNext = document.getElementById('search-next');

    // Clear previous highlights
    const text = container.textContent;
    searchMatches = [];
    searchIndex = -1;

    if (!query || query.length < 2) {
        searchCount.textContent = '';
        searchPrev.disabled = true;
        searchNext.disabled = true;
        return;
    }

    // Find all matches
    const lowerText = text.toLowerCase();
    const lowerQuery = query.toLowerCase();
    let pos = 0;
    while ((pos = lowerText.indexOf(lowerQuery, pos)) !== -1) {
        searchMatches.push(pos);
        pos += query.length;
    }

    if (searchMatches.length > 0) {
        searchCount.textContent = `${searchMatches.length} matches`;
        searchPrev.disabled = false;
        searchNext.disabled = false;
        navigateSearch(1);
    } else {
        searchCount.textContent = 'No matches';
        searchPrev.disabled = true;
        searchNext.disabled = true;
    }
}

function navigateSearch(direction) {
    if (searchMatches.length === 0) return;

    const container = document.getElementById('log-content');
    const searchCount = document.getElementById('search-count');
    const query = document.getElementById('log-search').value;

    searchIndex += direction;
    if (searchIndex >= searchMatches.length) searchIndex = 0;
    if (searchIndex < 0) searchIndex = searchMatches.length - 1;

    searchCount.textContent = `${searchIndex + 1}/${searchMatches.length}`;

    // Scroll to match position (approximate)
    const matchPos = searchMatches[searchIndex];
    const text = container.textContent;
    const linesBefore = text.substring(0, matchPos).split('\n').length;
    const lineHeight = parseInt(getComputedStyle(container).lineHeight) || 18;
    container.scrollTop = (linesBefore - 5) * lineHeight;
}

function setupCancelButton(buildId) {
    const btn = document.getElementById('cancel-build');
    if (!btn) return;
    btn.addEventListener('click', async () => {
        btn.disabled = true;
        btn.textContent = 'Cancelling...';
        try {
            await api.builds.cancel(buildId);
            await loadBuildInfo(buildId);
        } catch (error) {
            console.error('Failed to cancel build:', error);
            btn.textContent = 'Cancel Build';
            btn.disabled = false;
        }
    });
}

async function loadBuildInfo(buildId) {
    if (!isMounted) return;
    try {
        const build = await api.builds.get(buildId);
        if (!isMounted) return;
        currentBuildData = build;

        document.getElementById('build-pkg').textContent = build.pkgbase;
        document.getElementById('build-status').className = `status-badge ${build.status}`;
        document.getElementById('build-status').textContent = formatStatus(build.status);

        // Populate metadata bar
        populateMeta(build);

        // Show/hide cancel button for active builds
        const cancelBtn = document.getElementById('cancel-build');
        if (cancelBtn) {
            if (build.status === 'running' || build.status === 'pending') {
                cancelBtn.style.display = '';
                cancelBtn.disabled = false;
                cancelBtn.textContent = 'Cancel Build';
            } else {
                cancelBtn.style.display = 'none';
            }
        }

        // Show error banner for failed builds
        const errorBanner = document.getElementById('error-banner');
        if (errorBanner) {
            if (build.status === 'failed' && build.error_message) {
                document.getElementById('build-error-message').textContent = build.error_message;
                errorBanner.style.display = 'block';
            } else {
                errorBanner.style.display = 'none';
            }
        }

        // Show outputs panel for successful builds
        const outputsPanel = document.getElementById('outputs-panel');
        if (outputsPanel) {
            const outputs = build.outputs;
            if (build.status === 'success' && outputs && outputs.length > 0) {
                const list = document.getElementById('outputs-list');
                list.innerHTML = outputs.map(f => `<li>${escapeHtml(f)}</li>`).join('');
                outputsPanel.style.display = 'block';
            } else {
                outputsPanel.style.display = 'none';
            }
        }

        // Show stage for running builds
        const stageEl = document.getElementById('build-stage');
        const progressEl = document.getElementById('stage-progress');

        if (build.status === 'running' || build.status === 'pending') {
            stageEl.style.display = 'inline-block';
            progressEl.style.display = 'flex';

            if (build.current_stage) {
                updateStage(build.current_stage);
            }

            // Start streaming
            startStreaming(buildId);
        } else {
            stageEl.style.display = 'none';
            progressEl.style.display = 'none';
        }
    } catch (error) {
        document.getElementById('build-pkg').textContent = 'Unknown';
        document.getElementById('build-status').textContent = 'Error';
    }
}

function populateMeta(build) {
    const metaEl = document.getElementById('build-meta');
    if (!metaEl) return;

    // Started time
    const startedEl = document.getElementById('build-started');
    if (build.started_at) {
        startedEl.textContent = formatRelativeTime(build.started_at);
        startedEl.title = new Date(build.started_at).toLocaleString();
    }

    // Duration
    updateDuration(build);

    // Start live-ticking duration for running builds
    clearDurationInterval();
    if (build.status === 'running' || build.status === 'pending') {
        durationInterval = setInterval(() => {
            if (!isMounted) { clearDurationInterval(); return; }
            updateDuration(build);
        }, 1000);
    }

    // Commit
    const commitEl = document.getElementById('build-commit');
    if (build.commit) {
        const shortHash = build.commit.substring(0, 8);
        const aurUrl = `https://aur.archlinux.org/cgit/aur.git/commit/?h=${encodeURIComponent(build.pkgbase)}&id=${encodeURIComponent(build.commit)}`;
        commitEl.innerHTML = `<a href="${aurUrl}" target="_blank" rel="noopener" title="${escapeHtml(build.commit)}">${escapeHtml(shortHash)}</a>`;
    } else {
        commitEl.textContent = '-';
    }

    metaEl.style.display = 'flex';
}

function updateDuration(build) {
    const el = document.getElementById('build-duration');
    if (!el) return;
    el.textContent = formatDuration(build.started_at, build.finished_at);
}

async function loadLogs(buildId) {
    const container = document.getElementById('log-content');

    try {
        const logs = await api.builds.logs(buildId);
        if (logs) {
            container.innerHTML = highlightBuildLog(logs);
        } else {
            container.textContent = 'No logs available';
        }
        scrollToBottom(container, true);
    } catch (error) {
        // Logs might not be available yet
        container.textContent = 'Waiting for logs...';
    }
}

/**
 * Apply syntax highlighting to build log output.
 * Highlights common patterns: errors, warnings, success, headers, etc.
 */
function highlightBuildLog(text) {
    // Escape HTML first
    const escaped = text
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');

    // Split into lines and process each
    const lines = escaped.split('\n');
    const highlighted = lines.map(line => {
        // Section headers (=== ... ===, --- ... ---, ### ...)
        if (/^[=#-]{3,}.*[=#-]{3,}$/.test(line) || /^#{1,3}\s/.test(line)) {
            return `<span class="log-header">${line}</span>`;
        }

        // Arrow prompts (==&gt; ..., -&gt; ...)
        if (/^==&gt;|^-&gt;/.test(line)) {
            if (/ERROR|error|failed|Failed|FAILED/.test(line)) {
                return `<span class="log-error">${line}</span>`;
            }
            return `<span class="log-prompt">${line}</span>`;
        }

        // Errors (case-insensitive, various patterns)
        if (/\b(error|Error|ERROR)\b/.test(line) ||
            /\b(FAILED|failed|Failed)\b/.test(line) ||
            /^make\[\d+\]:.*Error/.test(line) ||
            /^error\[E\d+\]/.test(line)) {
            return `<span class="log-error">${line}</span>`;
        }

        // Warnings
        if (/\b(warning|Warning|WARNING)\b/.test(line) ||
            /\b(WARN|warn)\b/.test(line)) {
            return `<span class="log-warning">${line}</span>`;
        }

        // Success indicators
        if (/\b(SUCCESS|success|Finished|DONE|done|OK|passed|Passed)\b/.test(line) ||
            /\bFinished dev/.test(line) ||
            /^Build successful/.test(line)) {
            return `<span class="log-success">${line}</span>`;
        }

        // Package operations (pacman style)
        if (/^(installing|upgrading|removing|downloading)\s/i.test(line) ||
            /^:: (Retrieving|Installing|Processing|Synchronizing)/.test(line)) {
            return `<span class="log-package">${line}</span>`;
        }

        // Compiler/linker lines (Compiling, Linking, etc.)
        if (/^\s*(Compiling|Linking|Building|Running|Checking|Downloading)\s/.test(line)) {
            return `<span class="log-compile">${line}</span>`;
        }

        // Comments and muted lines
        if (/^\s*#/.test(line) || /^::/.test(line)) {
            return `<span class="log-muted">${line}</span>`;
        }

        return line;
    });

    return highlighted.join('\n');
}

function startStreaming(buildId) {
    const container = document.getElementById('log-content');
    let buffer = '';

    cleanup = api.streamLogs(
        buildId,
        // onMessage - log lines
        (data) => {
            // Accumulate data and highlight complete lines
            buffer += data;

            // Process complete lines
            const lastNewline = buffer.lastIndexOf('\n');
            if (lastNewline !== -1) {
                const completedLines = buffer.substring(0, lastNewline + 1);
                buffer = buffer.substring(lastNewline + 1);

                // Append highlighted content
                const highlighted = highlightBuildLog(completedLines);
                container.insertAdjacentHTML('beforeend', highlighted);
            } else if (!data.includes('\n')) {
                // No newline - append as-is for partial lines
                container.insertAdjacentHTML('beforeend', highlightBuildLog(data));
            }

            scrollToBottom(container);
        },
        // onClose
        () => {
            if (!isMounted) return;
            // Flush remaining buffer
            if (buffer) {
                container.insertAdjacentHTML('beforeend', highlightBuildLog(buffer));
                buffer = '';
            }
            // Stop duration ticking — will be recalculated from final data
            clearDurationInterval();
            // Connection closed - refresh build info
            loadBuildInfo(buildId);
        },
        // onStageChange
        (stage) => {
            updateStage(stage);
        }
    );
}

function updateStage(stage) {
    const stageEl = document.getElementById('build-stage');
    const progressEl = document.getElementById('stage-progress');

    // Track the last non-failed stage for failure indication
    if (stage !== 'failed') {
        lastActiveStage = stage;
    }

    // Update badge
    stageEl.textContent = formatStage(stage);
    stageEl.className = `stage-badge ${stage}`;

    // Update progress indicator
    const stages = ['starting', 'syncing_db', 'preparing', 'building', 'moving_to_repo', 'updating_index', 'complete'];

    if (stage === 'failed') {
        const failedIndex = lastActiveStage ? stages.indexOf(lastActiveStage) : 0;

        progressEl.querySelectorAll('.stage-item').forEach((item) => {
            const itemIndex = stages.indexOf(item.dataset.stage);
            item.classList.remove('active', 'completed', 'failed');

            if (itemIndex < failedIndex) {
                item.classList.add('completed');
            } else if (itemIndex === failedIndex) {
                item.classList.add('failed');
            }
        });
    } else {
        const currentIndex = stages.indexOf(stage);

        progressEl.querySelectorAll('.stage-item').forEach((item) => {
            const itemIndex = stages.indexOf(item.dataset.stage);
            item.classList.remove('active', 'completed', 'failed');

            if (itemIndex < currentIndex) {
                item.classList.add('completed');
            } else if (itemIndex === currentIndex) {
                item.classList.add('active');
            }
        });
    }
}

/** Track whether user is near the bottom of the log. If they scroll up, pause auto-scroll. */
function setupAutoScroll() {
    const container = document.querySelector('.log-container');
    if (!container) return;

    container.addEventListener('scroll', () => {
        const threshold = 50; // px from bottom
        const atBottom = container.scrollHeight - container.scrollTop - container.clientHeight < threshold;
        autoScroll = atBottom;
    });
}

function scrollToBottom(element, force = false) {
    if (!force && !autoScroll) return;
    element.scrollTop = element.scrollHeight;
}
