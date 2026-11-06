// GO audio alert — plays a distinct two-tone chirp when a live-path
// marker (APEX / Frontier) lands with verdict GO. Shadow / veto / FSM
// markers stay silent. Debounced so a marker burst rings once.
//
// Respects the same `ui.sound_on_fill` switch as the fill beep — one
// operator toggle controls all trading sounds.

import { playGoAlert } from './audio.js';
import { invoke, listen } from './tauri-bridge.js';

let _enabled = true;
let _lastPlayMs = 0;

const DEBOUNCE_MS = 3000;

function isLiveGo(m) {
    if (!m || m.display_only === true) return false;
    const src = (m.source || '').toLowerCase();
    if (src !== 'apex' && !src.startsWith('frontier')) return false;
    return (m.verdict || '').toUpperCase() === 'GO';
}

function onAlert(payload) {
    if (!_enabled || !payload || !Array.isArray(payload.markers)) return;
    if (!payload.markers.some(isLiveGo)) return;
    const now = Date.now();
    if (now - _lastPlayMs < DEBOUNCE_MS) return;
    _lastPlayMs = now;
    playGoAlert();
}

export async function initGoAlert() {
    try {
        const cfg = await invoke('get_config');
        _enabled = cfg?.ui?.sound_on_fill !== false;
    } catch (_) { /* default: enabled */ }
    listen('smarttrend-alert', (event) => onAlert(event.payload));
}
