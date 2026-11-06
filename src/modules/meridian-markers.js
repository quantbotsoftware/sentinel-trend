// MERIDIAN — pure formatting for the `meridian-state` payload: chart
// markers for the active symbol and one-line operator messages. No DOM,
// no bridge; imported by overlays.js (markers) and meridian-panel.js.

import { fmtMoney, fmtNum } from './fmt.js';

const COLOR_LONG = '#3FB950';
const COLOR_SHORT = '#F85149';
const COLOR_EXIT = '#D29922';

function isIntent(i) {
    return i && typeof i.symbol === 'string' && Number.isFinite(i.delta_shares) && Number.isFinite(i.bar_time) && i.bar_time > 0;
}

function kind(i) {
    const net = Number(i.net_after) || 0;
    if (net === 0) return 'exit';
    return i.delta_shares > 0 ? 'buy' : 'sell';
}

/** Chart markers (LightweightCharts shape) for intents on `symbol`. */
export function meridianMarkers(payload, symbol) {
    const list = Array.isArray(payload?.new_intents) ? payload.new_intents : [];
    if (!symbol) return [];
    return list.filter(isIntent).filter((i) => i.symbol === symbol).map((i) => {
        const k = kind(i);
        const sleeves = Array.isArray(i.sleeves) ? i.sleeves.join('+') : '';
        const shares = Math.abs(Math.round(i.delta_shares));
        const verb = i.delta_shares > 0 ? 'BUY' : 'SELL';
        return {
            time: i.bar_time,
            position: k === 'sell' || k === 'exit' ? 'aboveBar' : 'belowBar',
            color: k === 'buy' ? COLOR_LONG : k === 'sell' ? COLOR_SHORT : COLOR_EXIT,
            shape: k === 'buy' ? 'arrowUp' : k === 'sell' ? 'arrowDown' : 'circle',
            text: `MRD·${sleeves} ${verb} ${shares}`,
            price: i.fill_price,
            reason: `${i.time_et || ''} ${i.label || ''} — ${verb} ${shares} @ ${fmtNum(i.fill_price, 2)}${payload.execute ? '' : ' (shadow: no order sent)'}`.trim(),
            size: 2,
            is_real: false,
            display_only: !payload.execute,
            source: 'meridian',
            verdict: 'GO',
            direction: k === 'sell' ? 'Short' : 'Long',
        };
    });
}

/**
 * Chart markers the operator should see once MERIDIAN is the strategy:
 * MERIDIAN intents, real broker fills, and manual/bot order marks (★).
 * Legacy engine reference marks (FSM / APX / FRT …) are dropped — the
 * code that produces them is retired in phase 6.
 */
export function isOperatorMarker(m) {
    if (!m) return false;
    if (m.source === 'meridian' || m.is_real === true) return true;
    return typeof m.text === 'string' && m.text.startsWith('★');
}

/** Toast line: "MERIDIAN 10:30 · BUY 310 SPY @ 655.20 · A · LONG: …" */
export function formatIntentLine(i, execute) {
    if (!isIntent(i)) return '';
    const shares = Math.abs(Math.round(i.delta_shares));
    const verb = i.delta_shares > 0 ? 'BUY' : 'SELL';
    const tag = execute ? 'MERIDIAN' : 'MERIDIAN (shadow)';
    return `${tag} ${i.time_et || ''} · ${verb} ${shares} ${i.symbol} @ ${fmtNum(i.fill_price, 2)} · ${i.label || ''}`;
}

/** Sidebar status: "SPY +310 · QQQ 0 · eq $100,412 · 61 bars" */
export function formatStatusLine(snapshot) {
    if (!snapshot || typeof snapshot !== 'object') return '—';
    const net = snapshot.net_shares || {};
    const parts = Object.keys(net).sort().map((s) => {
        const n = Math.round(Number(net[s]) || 0);
        return `${s} ${n > 0 ? '+' : ''}${n}`;
    });
    const bars = Object.values(snapshot.bars_today || {}).map(Number).filter(Number.isFinite);
    const barsMax = bars.length ? Math.max(...bars) : 0;
    return `${parts.join(' · ') || '—'} · ${barsMax} bars`;
}
