// SentinelTrend — Tauri API single source of truth.
// Migration to Tauri 2.x will only touch this file:
//   - replace window.__TAURI__.tauri.invoke with @tauri-apps/api/core invoke
//   - replace window.__TAURI__.event with @tauri-apps/api/event
// Until then, callers MUST `import { invoke, listen, emit } from './tauri-bridge.js';`

const tauri = globalThis?.window?.__TAURI__ ?? globalThis?.__TAURI__;

if (!tauri || !tauri.tauri || !tauri.event) {
    throw new Error('tauri-bridge: Tauri runtime not available (are you running outside the Tauri shell?)');
}

export const invoke = (cmd, payload) => tauri.tauri.invoke(cmd, payload);

const _disposers = new Set();

export const listen = (event, handler) => {
    const p = tauri.event.listen(event, handler);
    p.then((fn) => _disposers.add(fn)).catch(() => { /* swallow */ });
    return p;
};

export function _disposeAllListeners() {
    for (const fn of _disposers) { try { fn(); } catch { /* noop */ } }
    _disposers.clear();
}

export const emit = (event, payload) => tauri.event.emit(event, payload);

// Re-export raw namespaces for advanced uses; AVOID consuming directly in new code.
export const __raw = tauri;
