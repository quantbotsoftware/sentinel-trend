// Premarket Ignition alerts — the auto-scan loop emits `scanner-ignition`
// with the symbols that entered the ignition list on this tick (score ≥
// `ignition.alert_min_score`, absent from the previous tick). One toast
// per entrant with move / RVOL / $-volume / headline, one chirp per
// batch (same `ui.sound_on_fill` switch as the fill beep and GO alert).

import { playGoAlert } from './audio.js';
import { emit } from './bus.js';
import { fmtNum } from './fmt.js';
import { showToast } from './toast.js';
import { invoke, listen } from './tauri-bridge.js';

let _soundEnabled = true;
let _lastPlayMs = 0;
const DEBOUNCE_MS = 3000;
const MAX_TOASTS = 5;

function fmtDollarVol(v) {
    const n = Number(v) || 0;
    if (n >= 1_000_000) return `$${(n / 1_000_000).toFixed(1)}M`;
    if (n >= 1_000) return `$${Math.round(n / 1_000)}K`;
    return `$${Math.round(n)}`;
}

export function formatIgnitionLine(a) {
    const sym = String(a?.symbol || '').toUpperCase();
    const chg = Number(a?.change_percent) || 0;
    const sign = chg >= 0 ? '+' : '';
    const head = String(a?.news_headline || '').trim();
    const story = head ? ` — ${head.length > 90 ? head.slice(0, 87) + '…' : head}` : '';
    const pills = Array.isArray(a?.catalyst_pills) ? a.catalyst_pills : [];
    // Tape-driven C-PM alerts (pill "LIVE") carry no RVOL; they show the
    // catalyst class and the suggested size instead of RVOL/score.
    if (pills[0] === 'LIVE') {
        const tags = pills.slice(1).join(' · ');
        return `IGNITION LIVE ${sym} ${sign}${fmtNum(chg, 1)}% · ${fmtDollarVol(a?.dollar_volume)}${tags ? ` · ${tags}` : ''}${story}`;
    }
    return `IGNITION ${sym} ${sign}${fmtNum(chg, 1)}% · RVOL ${fmtNum(Number(a?.rvol), 1)}x · ${fmtDollarVol(a?.dollar_volume)} · score ${Math.round(Number(a?.score) || 0)}${story}`;
}

/** Premarket only (04:00–09:30 ET, weekdays): scanner ignitions are noise in RTH. */
export function inPremarketEt(now = new Date()) {
    const et = new Date(now.toLocaleString('en-US', { timeZone: 'America/New_York' }));
    const day = et.getDay();
    if (day === 0 || day === 6) return false;
    const m = et.getHours() * 60 + et.getMinutes();
    return m >= 4 * 60 && m < 9 * 60 + 30;
}

function onIgnition(payload) {
    const alerts = Array.isArray(payload) ? payload : [];
    if (alerts.length === 0) return;
    // LIVE (C-PM headline) alerts carry their own window; scanner ones are premarket-only
    const live = alerts.some((a) => Array.isArray(a?.catalyst_pills) && a.catalyst_pills[0] === 'LIVE');
    if (!live && !inPremarketEt()) return;
    alerts.slice(0, MAX_TOASTS).forEach((a) => showToast(formatIgnitionLine(a), 'warning'));
    emit('scanner-ignition', { alerts });
    if (!_soundEnabled) return;
    const now = Date.now();
    if (now - _lastPlayMs < DEBOUNCE_MS) return;
    _lastPlayMs = now;
    playGoAlert();
}

export async function initScannerIgnition() {
    try {
        const cfg = await invoke('get_config');
        _soundEnabled = cfg?.ui?.sound_on_fill !== false;
    } catch (_) { /* default: enabled */ }
    listen('scanner-ignition', (event) => onIgnition(event.payload));
}
