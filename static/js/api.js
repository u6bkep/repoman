// API Client for Repoman

const API_BASE = '/api';

class ApiError extends Error {
    constructor(status, message) {
        super(message);
        this.status = status;
    }
}

async function request(method, path, body = null, signal = null) {
    const options = {
        method,
        headers: {},
    };

    if (body) {
        options.headers['Content-Type'] = 'application/json';
        options.body = JSON.stringify(body);
    }

    if (signal) {
        options.signal = signal;
    }

    const res = await fetch(`${API_BASE}${path}`, options);

    if (!res.ok) {
        const text = await res.text();
        throw new ApiError(res.status, text || `HTTP ${res.status}`);
    }

    // Handle empty responses
    const contentType = res.headers.get('content-type');
    if (contentType && contentType.includes('application/json')) {
        return res.json();
    }
    return res.text();
}

export const get = (path) => request('GET', path);
export const post = (path, body) => request('POST', path, body);
export const put = (path, body) => request('PUT', path, body);
export const patch = (path, body) => request('PATCH', path, body);
export const del = (path) => request('DELETE', path);

// Package operations
export const packages = {
    list: ({ signal } = {}) => request('GET', '/packages', null, signal),
    listOrphans: () => get('/packages/orphans'),
    get: (name) => get(`/packages/${encodeURIComponent(name)}`),
    add: (name, { asDependency = false, withDeps = false } = {}) => post('/packages', { name, as_dependency: asDependency, with_deps: withDeps }),
    remove: (name, deleteFiles = false) => del(`/packages/${encodeURIComponent(name)}?delete_files=${deleteFiles}`),
    approve: (name) => post(`/packages/${encodeURIComponent(name)}/approve`),
    reject: (name, reason = '') => post(`/packages/${encodeURIComponent(name)}/reject`, { reason }),
    build: (name) => post(`/packages/${encodeURIComponent(name)}/build`),
    pkgbuild: (name) => get(`/packages/${encodeURIComponent(name)}/pkgbuild`),
    diff: (name) => get(`/packages/${encodeURIComponent(name)}/diff`),
    checkUpdate: (name) => post(`/packages/${encodeURIComponent(name)}/check-update`),
    mark: (name, installReason) => post(`/packages/${encodeURIComponent(name)}/mark`, { install_reason: installReason }),
    deps: (name) => get(`/packages/${encodeURIComponent(name)}/deps`),
    approveAndBuild: () => post('/packages/approve-and-build'),
};

// Build operations
export const builds = {
    get: (id) => get(`/builds/${id}`),
    list: (pkgbase) => get(`/packages/${encodeURIComponent(pkgbase)}/builds`),
    logs: (id) => get(`/builds/${id}/logs`),
    cancel: (id) => post(`/builds/${id}/cancel`),
};

// AUR search
export const aur = {
    search: (query) => get(`/aur/search?q=${encodeURIComponent(query)}`),
    info: (name) => get(`/aur/info/${encodeURIComponent(name)}`),
};

// System operations
export const system = {
    status: () => get('/system/status'),
    rebuildImage: () => post('/system/image/rebuild'),
    cleanCache: () => post('/system/cache/clean'),
    cleanSources: () => post('/system/sources/clean'),
    cleanOrphans: () => post('/system/orphans/clean'),
    rebuildDb: () => post('/repo/rebuild-db'),
    checkUpdates: () => post('/packages/check-updates'),
    checkAndBuild: () => post('/packages/check-and-build'),
    // Container management
    listContainers: () => get('/system/containers'),
    pruneContainers: () => post('/system/containers/prune'),
    // Image management
    listImages: () => get('/system/images'),
    pruneImages: (keep = 3) => post(`/system/images/prune?keep=${keep}`),
    // Config
    configTemplate: () => get('/system/config/template'),
};

// Repository info
export const repo = {
    info: () => get('/repo'),
};

// WebSocket for log streaming
// onMessage receives log lines (strings)
// onStageChange receives stage change objects { stage: 'building' }
export function streamLogs(buildId, onMessage, onClose, onStageChange) {
    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = new WebSocket(`${protocol}//${location.host}/api/builds/${buildId}/logs/stream`);

    ws.onmessage = (e) => {
        const data = e.data;
        // Check if this is a JSON stage change message
        if (data.startsWith('{') && data.includes('"type"')) {
            try {
                const msg = JSON.parse(data);
                if (msg.type === 'stage' && onStageChange) {
                    onStageChange(msg.stage);
                    return;
                }
            } catch {
                // Not valid JSON, treat as log line
            }
        }
        // Regular log line
        onMessage(data);
    };
    ws.onclose = () => onClose?.();
    ws.onerror = (e) => console.error('WebSocket error:', e);

    return () => ws.close();
}
