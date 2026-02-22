// Build Log component
import * as api from '../api.js';
import { formatStatus, formatStage } from './utils.js';

let cleanup = null;
let currentBuildId = null;
let searchIndex = -1;
let searchMatches = [];
let isMounted = false;

export async function init(params) {
    cleanup = null;
    currentBuildId = null;
    searchIndex = -1;
    searchMatches = [];
    isMounted = true;

    const { id } = params;
    currentBuildId = id;

    setupLogControls(id);
    await loadBuildInfo(id);
    await loadLogs(id);

    // Return cleanup function
    return () => {
        isMounted = false;
        if (cleanup) {
            cleanup();
            cleanup = null;
        }
    };
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

async function loadBuildInfo(buildId) {
    if (!isMounted) return;
    try {
        const build = await api.builds.get(buildId);
        if (!isMounted) return;

        document.getElementById('build-pkg').textContent = build.pkgbase;
        document.getElementById('build-status').className = `status-badge ${build.status}`;
        document.getElementById('build-status').textContent = formatStatus(build.status);

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

async function loadLogs(buildId) {
    const container = document.getElementById('log-content');

    try {
        const logs = await api.builds.logs(buildId);
        if (logs) {
            container.innerHTML = highlightBuildLog(logs);
        } else {
            container.textContent = 'No logs available';
        }
        scrollToBottom(container);
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

    // Update badge
    stageEl.textContent = formatStage(stage);
    stageEl.className = `stage-badge ${stage}`;

    // Update progress indicator
    const stages = ['starting', 'syncing_db', 'preparing', 'building', 'moving_to_repo', 'updating_index', 'complete'];
    const currentIndex = stages.indexOf(stage);

    progressEl.querySelectorAll('.stage-item').forEach((item, index) => {
        const itemStage = item.dataset.stage;
        const itemIndex = stages.indexOf(itemStage);

        item.classList.remove('active', 'completed', 'failed');

        if (stage === 'failed') {
            if (itemIndex < currentIndex || itemStage === stage) {
                item.classList.add('failed');
            }
        } else if (itemIndex < currentIndex) {
            item.classList.add('completed');
        } else if (itemIndex === currentIndex) {
            item.classList.add('active');
        }
    });

    // Hide failed stage item if not failed
    const failedStageItem = progressEl.querySelector('[data-stage="failed"]');
    if (failedStageItem) {
        failedStageItem.style.display = stage === 'failed' ? 'block' : 'none';
    }
}

function scrollToBottom(element) {
    element.scrollTop = element.scrollHeight;
}
