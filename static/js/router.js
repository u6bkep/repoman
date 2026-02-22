// Simple hash-based router for Repoman

const routes = new Map();
let currentCleanup = null;

export function register(pattern, handler) {
    routes.set(pattern, handler);
}

export function navigate(path) {
    window.location.hash = path;
}

function matchRoute(hash) {
    const path = hash.slice(1) || '/';  // Remove #

    for (const [pattern, handler] of routes) {
        const params = matchPattern(pattern, path);
        if (params !== null) {
            return { handler, params };
        }
    }

    return null;
}

function matchPattern(pattern, path) {
    // Convert pattern like '/packages/:name' to regex
    const paramNames = [];
    const regexStr = pattern.replace(/:([^/]+)/g, (_, name) => {
        paramNames.push(name);
        return '([^/]+)';
    });

    const regex = new RegExp(`^${regexStr}$`);
    const match = path.match(regex);

    if (!match) return null;

    const params = {};
    paramNames.forEach((name, i) => {
        params[name] = decodeURIComponent(match[i + 1]);
    });

    return params;
}

async function handleRoute() {
    // Clean up previous route
    if (currentCleanup) {
        currentCleanup();
        currentCleanup = null;
    }

    const hash = window.location.hash || '#/';
    const match = matchRoute(hash);

    // Update active nav link
    document.querySelectorAll('.nav-links a').forEach(link => {
        const href = link.getAttribute('href');
        if (href === hash || (href !== '#/' && (hash === href || hash.startsWith(href + '/')))) {
            link.classList.add('active');
        } else {
            link.classList.remove('active');
        }
    });

    if (match) {
        try {
            currentCleanup = await match.handler(match.params);
        } catch (error) {
            console.error('Route error:', error);
            showError(error.message);
        }
    } else {
        showError('Page not found');
    }
}

function showError(message) {
    const app = document.getElementById('app');
    const template = document.getElementById('tmpl-error');
    const content = template.content.cloneNode(true);
    content.getElementById('error-message').textContent = message;
    app.innerHTML = '';
    app.appendChild(content);
}

export function init() {
    window.addEventListener('hashchange', handleRoute);
    handleRoute();
}

// Helper to render a template into #app
export function render(templateId) {
    const app = document.getElementById('app');
    const template = document.getElementById(templateId);
    if (!template) {
        console.error(`Template not found: ${templateId}`);
        return app;
    }
    const content = template.content.cloneNode(true);
    app.innerHTML = '';
    app.appendChild(content);
    return app;
}
