// Tradeability badge (2026-07-04) — manual-trading confidence layer.
//
// Polls `get_tradeability` for the active symbol and renders a verdict
// badge in the chart header:
//   APEX WINDOW  — inside the house strategy's own entry conditions
//   TRADEABLE    — liquidity/safety gates pass (RTH, spread, halt, data)
//   NOT TRADEABLE— one or more gates fail (tooltip says which)
//   CLOSED       — market session closed
//
// The verdict is computed server-side from the SAME physical gates
// APEX trades through — it is evidence, not advice.

import { invoke } from './tauri-bridge.js';

const $id = (id) => document.getElementById(id);

const POLL_MS = 15_000;
const _intervals = [];

function activeSymbol() {
    const sym = $id('st-header-symbol')?.textContent?.trim();
    return sym && sym !== '---' ? sym : null;
}

function render(payload) {
    const el = $id('st-tradeability');
    if (!el) return;

    el.classList.remove(
        'st-tradeability--go',
        'st-tradeability--apex',
        'st-tradeability--no',
        'st-tradeability--off'
    );

    if (!payload) {
        el.textContent = '---';
        el.classList.add('st-tradeability--off');
        el.title = 'Tradeability: no data';
        return;
    }

    if (payload.session !== 'rth' && !payload.tradeable) {
        el.textContent = payload.session === 'closed' ? 'CLOSED' : payload.session.toUpperCase();
        el.classList.add('st-tradeability--off');
    } else if (payload.apex_window) {
        el.textContent = 'APEX WINDOW';
        el.classList.add('st-tradeability--apex');
    } else if (payload.tradeable) {
        el.textContent = 'TRADEABLE';
        el.classList.add('st-tradeability--go');
    } else {
        el.textContent = 'NOT TRADEABLE';
        el.classList.add('st-tradeability--no');
    }

    const lines = (payload.checks || []).map(
        (c) => `${c.passed ? 'OK ' : 'FAIL'} ${c.name}: ${c.detail}`
    );
    el.title = `Manual-trading conditions — ${payload.symbol}\n${lines.join('\n')}`;
}

async function refresh() {
    const symbol = activeSymbol();
    if (!symbol) {
        render(null);
        return;
    }
    try {
        const payload = await invoke('get_tradeability', { symbol });
        render(payload);
    } catch (err) {
        console.warn('[tradeability] refresh failed:', err);
        render(null);
    }
}

export function initTradeability() {
    refresh();
    _intervals.push(setInterval(refresh, POLL_MS));
}

export function disposeTradeability() {
    for (const id of _intervals.splice(0)) clearInterval(id);
}
