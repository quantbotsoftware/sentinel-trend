import { on } from './bus.js';
import { escapeHtml } from './esc.js';
import { getCandleSeries, getMainChart } from './chart.js';
import { isOperatorMarker, meridianMarkers } from './meridian-markers.js';

import { invoke, listen } from './tauri-bridge.js';

let _markers = [];
let _btMarkers = [];
let _visible = true;
let _currentSymbol = '';
let _currentTF = '';
let _tooltipEl = null;
let _loadVersion = 0;

// ─── Marker Storage ──────────────────────────────────────────────────
function dedupKey(m) {
    return `${timeKey(m.time)}|${m.position}|${m.shape}|${m.text}`;
}

function mergeMarkers(existing, incoming) {
    const seen = new Set(existing.map(dedupKey));
    const merged = [...existing];
    for (const m of incoming) {
        const k = dedupKey(m);
        if (!seen.has(k)) {
            seen.add(k);
            merged.push(m);
        }
    }
    merged.sort((a, b) => a.time - b.time);
    return merged;
}

function combinedMarkers() {
    const all = [..._markers.filter(isOperatorMarker), ..._btMarkers];
    all.sort((a, b) => a.time - b.time);
    return all;
}

function applyMarkers() {
    const series = getCandleSeries();
    if (!series) return;

    if (!_visible) {
        try { series.setMarkers([]); } catch (_) { /* noop */ }
        return;
    }

    try {
        series.setMarkers(combinedMarkers());
    } catch (err) {
        console.error('[overlays] setMarkers failed:', err);
    }
}

// ─── Load historical markers from backend ────────────────────────────
async function loadMarkers(symbol, tf) {
    const loadVersion = ++_loadVersion;
    _currentSymbol = symbol;
    _currentTF = tf;
    _markers = [];
    applyMarkers();

    try {
        const raw = await invoke('get_markers', { symbol, timeframe: tf });
        if (loadVersion !== _loadVersion) return;
        const fetched = Array.isArray(raw) ? raw : [];
        _markers = fetched
            .filter(isActionableChartMarker)
            .map((m) => ({
                ...m,
                size: typeof m.size === 'number' ? m.size : 1,
                is_real: m.is_real === true,
                display_only: m.display_only === true,
                reason: normalizeReason(m.reason, m.text),
            }));
        applyMarkers();
    } catch (err) {
        if (loadVersion !== _loadVersion) return;
        console.error('[overlays] get_markers failed:', err);
    }
}

// ─── Live marker ingestion from sentinel events ──────────────────────
function ingestAlert(payload) {
    if (!payload || !payload.markers || payload.markers.length === 0) return;
    if (payload.symbol !== _currentSymbol) return;

    const normTf = (t) => {
        switch (t) {
            case '1Min': return '1m';
            case '5Min': return '5m';
            case '10Min': return '10m';
            case '15Min': return '15m';
            case '30Min': return '30m';
            case '1Hour': return '1h';
            case '4Hour': return '4h';
            case '1Day': case '1D': return '1d';
            default: return t;
        }
    };
    // F3b — live-path / Frontier markers are TF-agnostic (`any`).
    const tfAny = payload.timeframe === 'any' || payload.phase === 'Frontier' || payload.phase === 'LivePath';
    if (!tfAny && normTf(payload.timeframe) !== normTf(_currentTF)) return;

    const transition = formatTransition(payload.prev_phase, payload.phase);
    const incoming = payload.markers
        .filter(isActionablePayloadMarker)
        .map((m) => livePathChartMarker(m, transition));
    if (incoming.length === 0) return;

    _markers = mergeMarkers(_markers, incoming);
    applyMarkers();
}

function livePathChartMarker(m, transition) {
    const source = (m.source || 'fsm').toLowerCase();
    const verdict = (m.verdict || (m.display_only ? 'VETO' : source === 'fsm' ? 'FSM' : 'GO')).toUpperCase();
    const conv = typeof m.conviction === 'number' ? m.conviction : (m.score || 0) / 100;
    const short = m.direction === 'Short';
    const setup = m.setup || '';
    const live = source === 'apex' || source === 'frontier' || source.startsWith('frontier');
    let text;
    let color;
    let shape;
    let displayOnly = m.display_only === true;
    let reason;

    if (m.kind === 'Exit' && verdict === 'HOLD') {
        text = 'HOLD';
        color = '#D29922';
        shape = 'circle';
        displayOnly = true;
        reason = `HOLD — winner protected at +${conv.toFixed(2)}R; breakeven armed, do NOT sell manually`;
    } else if (m.kind === 'Exit') {
        text = live ? 'GO SELL' : 'FSM SELL';
        color = '#F85149';
        shape = 'arrowDown';
        reason = 'EXIT — flatten here (simulation unless ★ BOT SELL also present)';
    } else if (verdict === 'SHADOW') {
        text = `FRT·${(setup || 'TAPE').slice(0, 4).toUpperCase()} SHADOW`;
        color = '#BC8CFF';
        shape = 'circle';
        displayOnly = true;
        reason = 'SHADOW — Frontier candidate (no order); manual OK for validation';
    } else if (live && verdict === 'GO') {
        const prefix = source === 'apex'
            ? 'APX'
            : `FRT·${(setup || 'TAPE').slice(0, 4).toUpperCase()}`;
        text = short ? `${prefix} ${conv.toFixed(2)} GO SELL·SHORT` : `${prefix} ${conv.toFixed(2)} GO`;
        color = short ? '#F85149' : '#58A6FF';
        shape = short ? 'arrowDown' : 'arrowUp';
        reason = `${prefix} GO conv=${conv.toFixed(2)} setup=${setup || '—'} — live path can place orders`;
    } else if (live && verdict === 'VETO') {
        text = `${source === 'apex' ? 'APX' : 'FRT'} VETO`;
        color = '#6E7681';
        shape = 'circle';
        displayOnly = true;
        reason = `${source} VETO — live path rejected`;
    } else {
        text = m.display_only ? 'FSM SKIP' : 'FSM';
        color = '#6E7681';
        shape = 'circle';
        displayOnly = true;
        reason = 'FSM reference — does not place orders in apex_only';
    }

    return {
        time: m.bar_time,
        position: short || m.kind === 'Exit' ? 'aboveBar' : 'belowBar',
        color,
        shape,
        text,
        score: m.score,
        price: m.price,
        reason,
        transition,
        direction: m.direction || '',
        size: live && verdict === 'GO' ? 2 : 1,
        is_real: false,
        display_only: displayOnly,
        source,
        setup,
        conviction: conv,
        verdict,
    };
}

function isActionablePayloadMarker(marker) {
    if (!marker || typeof marker.kind !== 'string') return false;
    if (marker.kind === 'Entry') {
        // F3b — shorts (TRUE_RESUME fade) are visible.
        return marker.direction === 'Long' || marker.direction === 'Short';
    }
    return marker.kind === 'Exit';
}

function isActionableChartMarker(marker) {
    if (!marker || typeof marker.text !== 'string') return false;
    const t = marker.text;
    if (t === 'GO BUY' || t === 'SKIP' || t === 'FSM' || t === 'FSM SKIP' || t === 'FSM BUY' || t === 'FSM SELL') return true;
    if (t === 'HOLD' || t === 'GO SELL' || t.includes('GO SELL·SHORT')) return true;
    if (t === 'ST BUY' || t === 'ST EXIT' || t === 'sig↑' || t === 'sig↓') return true;
    if (t.startsWith('★ BOT BUY') || t.startsWith('★ BOT SELL')) return true;
    if (t.startsWith('APX') || t.startsWith('FRT') || t.includes(' SHADOW')) return true;
    if (t === '' && (marker.shape === 'arrowUp' || marker.shape === 'arrowDown')) return true;
    return false;
}

function normalizeReason(reason, text) {
    if (typeof reason === 'string' && reason.length > 0) return reason;
    if (text === 'GO BUY' || text === 'FSM BUY' || text === 'ST BUY') {
        return 'GATES PASSED — FSM would enter here (not a broker fill)';
    }
    if (text === 'FSM' || text === 'FSM SKIP') return 'FSM reference — does not place orders in apex_only';
    if (text === 'SKIP') return 'VETOED — gates rejected; do NOT buy from this marker';
    if (text === 'HOLD') return 'HOLD — winner protected; breakeven armed';
    if (text === 'GO SELL' || text === 'FSM SELL' || text === 'ST EXIT') return 'EXIT — flatten here';
    if (typeof text === 'string' && text.startsWith('★ BOT BUY')) return 'REAL trade — bot opened a position';
    if (typeof text === 'string' && text.startsWith('★ BOT SELL')) return 'REAL trade — bot closed a position';
    if (text === 'sig↑' || text === 'sig↓') return 'FSM simulation signal';
    return '';
}

function trustBadgeHtml(hit) {
    if (hit.is_real) {
        return '<span class="st-marker-tooltip-badge st-marker-badge--real">★ REAL TRADE</span>';
    }
    if (hit.verdict === 'HOLD' || hit.text === 'HOLD') {
        return '<span class="st-marker-tooltip-badge st-marker-badge--go">HOLD — WINNER PROTECTED</span>';
    }
    if (hit.verdict === 'SHADOW' || (typeof hit.text === 'string' && hit.text.includes('SHADOW'))) {
        return '<span class="st-marker-tooltip-badge st-marker-badge--skip">SHADOW — NO ORDER</span>';
    }
    if (hit.display_only || hit.text === 'SKIP' || hit.text === 'FSM' || hit.text === 'FSM SKIP') {
        return '<span class="st-marker-tooltip-badge st-marker-badge--skip">REFERENCE — DO NOT TREAT AS GO</span>';
    }
    return '<span class="st-marker-tooltip-badge st-marker-badge--go">GO — LIVE PATH</span>';
}

function formatTransition(prevPhase, phase) {
    if (!prevPhase || !phase || prevPhase === phase) return '';
    return `${prevPhase} -> ${phase}`;
}

function timeKey(time) {
    if (time == null) return '';
    if (typeof time === 'number' || typeof time === 'string') return String(time);
    if (typeof time === 'object' && typeof time.timestamp === 'number') return String(time.timestamp);
    return String(time);
}

// ─── BONUS 1: Marker Tooltip on Hover ────────────────────────────────
function createTooltip() {
    const el = document.createElement('div');
    el.className = 'st-marker-tooltip';
    document.getElementById('st-chart').appendChild(el);
    return el;
}

function setupTooltip() {
    const chart = getMainChart();
    if (!chart) return;

    _tooltipEl = createTooltip();

    chart.subscribeCrosshairMove((param) => {
        const combined = combinedMarkers();
        if (!_tooltipEl || !_visible || combined.length === 0) {
            if (_tooltipEl) _tooltipEl.style.display = 'none';
            return;
        }

        if (!param.point || !param.time) {
            _tooltipEl.style.display = 'none';
            return;
        }

        const hit = combined.find((m) => timeKey(m.time) === timeKey(param.time));
        if (!hit) {
            _tooltipEl.style.display = 'none';
            return;
        }

        const priceStr = hit.price != null ? `$${hit.price.toFixed(2)}` : '';
        const scoreStr = hit.score != null && hit.score > 0 ? hit.score.toFixed(1) : '';
        const reasonStr = hit.reason ? escapeHtml(hit.reason) : '';
        const transitionStr = hit.transition ? escapeHtml(hit.transition) : '';
        const label = hit.text || (hit.display_only ? 'SKIP' : 'GO BUY');
        _tooltipEl.innerHTML =
            trustBadgeHtml(hit) +
            `<span class="st-marker-tooltip-kind" style="color:${hit.color}">${escapeHtml(label)}</span>` +
            (priceStr ? `<span class="st-marker-tooltip-price">${priceStr}</span>` : '') +
            (scoreStr ? `<span class="st-marker-tooltip-score">Score: ${scoreStr}</span>` : '') +
            (reasonStr ? `<span class="st-marker-tooltip-score">Why: ${reasonStr}</span>` : '') +
            (transitionStr ? `<span class="st-marker-tooltip-score">Phase: ${transitionStr}</span>` : '');

        const chartArea = document.getElementById('st-chart');
        const rect = chartArea.getBoundingClientRect();
        const x = Math.min(param.point.x + 12, rect.width - 140);
        const y = Math.max(param.point.y - 60, 4);

        _tooltipEl.style.left = `${x}px`;
        _tooltipEl.style.top = `${y}px`;
        _tooltipEl.style.display = 'flex';
    });
}

// ─── BONUS 2: Marker Visibility Toggle ───────────────────────────────
const LS_KEY = 'st-markers-visible';

function initToggle() {
    const stored = localStorage.getItem(LS_KEY);
    _visible = stored !== 'false';

    const btn = document.getElementById('st-markers-toggle');
    if (!btn) return;

    updateToggleBtn(btn);

    btn.addEventListener('click', () => {
        _visible = !_visible;
        localStorage.setItem(LS_KEY, String(_visible));
        updateToggleBtn(btn);
        applyMarkers();
    });
}

function updateToggleBtn(btn) {
    btn.textContent = 'MK';
    btn.classList.toggle('st-tf-btn--active', _visible);
    btn.title = _visible ? 'Hide markers' : 'Show markers';
}

// ─── Event Bus Listeners ─────────────────────────────────────────────
function setupBusListeners() {
    on('chart-loaded', (data) => {
        if (data && data.symbol && data.tf) {
            loadMarkers(data.symbol, data.tf);
        }
    });

    on('tf-changed', (data) => {
        if (data && data.tf && _currentSymbol) {
            loadMarkers(_currentSymbol, data.tf);
        }
    });

    on('symbol-changed', (data) => {
        const sym = typeof data === 'string' ? data : data?.symbol;
        if (typeof sym === 'string' && sym.length > 0 && _currentTF) {
            loadMarkers(sym, _currentTF);
        }
    });
}

// ─── Tauri Event Listeners ───────────────────────────────────────────
function setupTauriListeners() {
    listen('smarttrend-alert', (event) => {
        ingestAlert(event.payload);
    });
    listen('meridian-state', (event) => { // MERIDIAN intents — TF-agnostic, keyed by bar time
        const incoming = meridianMarkers(event.payload, _currentSymbol);
        if (incoming.length === 0) return;
        _markers = mergeMarkers(_markers, incoming);
        applyMarkers();
    });
}

// ─── Backtest Marker Integration ─────────────────────────────────────
export function getLiveMarkers() {
    return _markers;
}

export function setBacktestMarkers(btMarkers) {
    _btMarkers = Array.isArray(btMarkers) ? btMarkers : [];
    applyMarkers();
}

// ─── Dispose (called from core.js beforeunload aggregator) ──────────
export function disposeOverlays() {
    _tooltipEl?.remove();
    _tooltipEl = null;
    _markers = [];
    _btMarkers = [];
}

// ─── Public Init ─────────────────────────────────────────────────────
export async function initOverlays() {
    initToggle();
    setupBusListeners();
    setupTauriListeners();
    setupTooltip();
    try {
        const sym = await invoke('get_active_symbol');
        const cfg = await invoke('get_config');
        if (sym && cfg?.ui?.default_timeframe) loadMarkers(sym, cfg.ui.default_timeframe);
    } catch (_) { /* chart-loaded handles cold start */ }
}
