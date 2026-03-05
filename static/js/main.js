// Repoman - Main entry point
import { register, init as initRouter, render } from './router.js';
import { connect } from './events.js';
import * as dashboard from './components/dashboard.js';
import * as packageList from './components/package-list.js';
import * as packageDetail from './components/package-detail.js';
import * as addPackage from './components/add-package.js';
import * as buildLog from './components/build-log.js';
import * as buildsPage from './components/builds.js';
import * as settings from './components/settings.js';

// Register routes
register('/', async () => {
    render('tmpl-dashboard');
    await dashboard.init();
});

register('/packages', async () => {
    render('tmpl-package-list');
    await packageList.init();
});

register('/packages/:name', async (params) => {
    render('tmpl-package-detail');
    await packageDetail.init(params);
});

register('/add', async () => {
    render('tmpl-add-package');
    await addPackage.init();
});

register('/builds', async () => {
    render('tmpl-builds');
    return await buildsPage.init();
});

register('/builds/:id', async (params) => {
    render('tmpl-build-log');
    return await buildLog.init(params);
});

register('/settings', async () => {
    render('tmpl-settings');
    await settings.init();
});

// Initialize router
initRouter();

// Connect SSE event bus — fire and forget; components opt in via window.addEventListener('repoman:status', ...)
connect();

console.log('Repoman UI initialized');
