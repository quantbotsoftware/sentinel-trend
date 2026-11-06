// MERIDIAN operator surface: listens to `meridian-state`, updates the
// sidebar status block, toasts every new intent with the rule that fired
// (so a discretionary trader can verify it on the candles) and rings the
// GO chirp (debounced). Chart markers are handled by overlays.js via
// meridian-markers.js.

import { playGoAlert } from './audio.js';
import { escapeHtml } from './esc.js';
import { formatIntentLine, formatStatusLine } from './meridian-markers.js';
import { showToast } from './toast.js';
import { invoke, listen } from './tauri-bridge.js';

const DEBOUNCE_MS = 3000;
const MAX_TOASTS = 4;

let _soundOn = true;
let _lastPlayMs = 0;

// `live_risk` is serde-tagged: "Normal" | { Half: { reason } } | { Halted: { reason } }
function riskLabel(rail) {
    if (!rail || rail === 'Normal') return null;
    if (rail.Halted) return { level: 'halt', text: 'RISK HALT', reason: rail.Halted.reason || '' };
    if (rail.Half) return { level: 'half', text: 'RISK ½', reason: rail.Half.reason || '' };
    return null;
}

function render(payload) {
    const status = document.getElementById('st-meridian-status');
    const last = document.getElementById('st-meridian-last');
    const mode = document.getElementById('st-meridian-mode');
    if (status) status.textContent = formatStatusLine(payload?.snapshot);
    if (mode) {
        const rail = riskLabel(payload?.live_risk);
        // `host` = which process drives: the headless daemon or this window
        const host = payload?.host === 'daemon' ? ' · daemon' : '';
        mode.textContent = (payload?.execute ? 'EXECUTE' : 'SHADOW') + host + (rail ? ` · ${rail.text}` : '');
        mode.title = rail?.reason || (payload?.host === 'daemon' ? 'executat de meridian-daemon (systemd); fereastra doar afișează' : '');
        mode.classList.toggle('st-meridian-mode--exec', !!payload?.execute && !rail);
        mode.classList.toggle('st-meridian-mode--half', rail?.level === 'half');
        mode.classList.toggle('st-meridian-mode--halt', rail?.level === 'halt');
    }
    const intents = Array.isArray(payload?.snapshot?.intents_today) ? payload.snapshot.intents_today : [];
    if (last) {
        const tail = intents[intents.length - 1];
        last.innerHTML = tail ? escapeHtml(formatIntentLine(tail, payload.execute)) : 'no intent yet today';
    }
    const errs = Array.isArray(payload?.errors) ? payload.errors : [];
    const errEl = document.getElementById('st-meridian-errors');
    if (errEl) {
        errEl.textContent = errs.length ? errs.join(' · ') : '';
        errEl.classList.toggle('st-hidden', errs.length === 0);
    }
}

function onState(payload) {
    render(payload);
    const fresh = Array.isArray(payload?.new_intents) ? payload.new_intents : [];
    if (fresh.length === 0) return;
    fresh.slice(0, MAX_TOASTS).forEach((i) => showToast(formatIntentLine(i, payload.execute), payload.execute ? 'success' : 'warning'));
    if (!_soundOn) return;
    const now = Date.now();
    if (now - _lastPlayMs < DEBOUNCE_MS) return;
    _lastPlayMs = now;
    playGoAlert();
}

export async function initMeridianPanel() {
    try {
        const cfg = await invoke('get_config');
        _soundOn = cfg?.ui?.sound_on_fill !== false;
        const mode = document.getElementById('st-meridian-mode');
        if (mode) mode.textContent = cfg?.meridian?.execute ? 'EXECUTE' : 'SHADOW';
    } catch (_) { /* defaults */ }
    listen('meridian-state', (event) => onState(event.payload));
    try {
        const last = await invoke('get_meridian_state');
        if (last) render(last);
    } catch (_) { /* driver not started yet */ }
}
