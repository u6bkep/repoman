// Global SSE event manager for Repoman
// Connects to /api/events, parses JSON, dispatches typed CustomEvents on window

let eventSource = null;

export function connect() {
    if (eventSource) return;

    console.log('[events] Connecting to /api/events...');
    eventSource = new EventSource('/api/events');

    eventSource.onopen = () => {
        console.log('[events] Connected');
    };

    eventSource.onmessage = (e) => {
        let parsed;
        try {
            parsed = JSON.parse(e.data);
        } catch {
            console.warn('[events] Failed to parse SSE data:', e.data);
            return;
        }
        window.dispatchEvent(new CustomEvent('repoman:status', { detail: parsed }));
    };

    eventSource.onerror = () => {
        // EventSource auto-reconnects. Log reconnect state but don't close — let the browser handle it.
        if (eventSource.readyState === EventSource.CONNECTING) {
            console.log('[events] Reconnecting...');
        } else if (eventSource.readyState === EventSource.CLOSED) {
            console.log('[events] Connection closed, will retry');
        }
    };
}

export function disconnect() {
    if (eventSource) {
        console.log('[events] Disconnecting');
        eventSource.close();
        eventSource = null;
    }
}
