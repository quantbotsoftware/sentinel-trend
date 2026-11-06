// APEX trade plan overlay (2026-07-04) — "the message on the chart".
//
// Polls `get_trade_plan` for the active symbol/timeframe and renders:
//   * three price lines on the candle series — ENTRY (gold), STOP
//     (red), TARGET (green) — exactly where APEX would enter and exit;
//   * a compact panel with the calculations: risk per share, share
//     count for the strategy's risk budget, R:R, score, max hold;
//   * when there is NO valid entry, the FIRST failing gate verbatim —
//     the honest answer to "why shouldn't I buy right now?".
//
// The plan comes from the same `evaluate_entry_explained` gate chain
// the live strategy trades through — evidence, never improvisation.

import { invoke } from './tauri-bridge.js';
import { on } from './bus.js';
import { getCandleSeries } from './chart.js';
import { escapeHtml } from './esc.js';
import { fmtNum, fmtRvol } from './fmt.js';

const $id = (id) => document.getElementById(id);

const POLL_MS = 15_000;
const _intervals = [];
const _lines = [];

function activeSymbol() {
    const sym = $id('st-header-symbol')?.textContent?.trim();
    return sym && sym !== '---' ? sym : null;
}

function activeTf() {
    const btn = document.querySelector('.st-tf-btn--active[data-tf]');
    const tf = btn?.dataset?.tf || '5m';
    // The plan is only defined on intraday execution timeframes.
    if (tf === '1m' || tf === '15m') return tf;
    return '5m';
}

function clearLines() {
    const series = getCandleSeries();
    if (series) {
        for (const line of _lines.splice(0)) {
            try { series.removePriceLine(line); } catch { /* series rebuilt */ }
        }
    } else {
        _lines.length = 0;
    }
}

function drawLines(plan) {
    const series = getCandleSeries();
    if (!series || !window.LightweightCharts) return;
    const style = LightweightCharts.LineStyle;
    const mk = (price, color, title, lineStyle) =>
        series.createPriceLine({
            price,
            color,
            lineWidth: 2,
            lineStyle,
            axisLabelVisible: true,
            title,
        });
    _lines.push(mk(plan.entry, '#D29922', 'APEX ENTRY', style.Solid));
    _lines.push(mk(plan.stop, '#F85149', 'STOP', style.Dashed));
    _lines.push(mk(plan.target, '#3FB950', 'TARGET', style.Dashed));
}

function renderPanel(payload) {
    const el = $id('st-trade-plan');
    if (!el) return;
    if (!payload) {
        el.classList.add('st-trade-plan--hidden');
        return;
    }
    el.classList.remove('st-trade-plan--hidden');
    el.classList.toggle('st-trade-plan--live', payload.has_plan);

    if (payload.has_plan) {
        const sideLabel = payload.side === 'long' ? 'LONG ▲' : 'SHORT ▼';
        const sideCls = payload.side === 'long' ? 'st-trade-plan__long' : 'st-trade-plan__short';
        el.innerHTML = `
            <div class="st-trade-plan__title">APEX PLAN — <span class="${sideCls}">${escapeHtml(sideLabel)}</span></div>
            <div class="st-trade-plan__row"><span>ENTRY</span><b class="st-trade-plan__gold">$${escapeHtml(fmtNum(payload.entry))}</b></div>
            <div class="st-trade-plan__row"><span>STOP</span><b class="st-trade-plan__short">$${escapeHtml(fmtNum(payload.stop))}</b></div>
            <div class="st-trade-plan__row"><span>TARGET</span><b class="st-trade-plan__long">$${escapeHtml(fmtNum(payload.target))}</b></div>
            <div class="st-trade-plan__row"><span>RISC/ACȚ</span><b>$${escapeHtml(fmtNum(payload.risk_per_share))}</b></div>
            <div class="st-trade-plan__row"><span>ACȚIUNI</span><b>${escapeHtml(String(payload.shares ?? 0))} (risc $${escapeHtml(fmtNum(payload.risk_dollars, 0))})</b></div>
            <div class="st-trade-plan__row"><span>R:R</span><b>1 : ${escapeHtml(fmtNum(payload.rr, 1))} (+$${escapeHtml(fmtNum(payload.reward_dollars, 0))})</b></div>
            <div class="st-trade-plan__row"><span>SCOR</span><b>${escapeHtml(fmtNum(payload.score, 0))}/100 · max ${escapeHtml(String(payload.max_hold_minutes ?? 0))} min</b></div>`;
    } else {
        el.innerHTML = `
            <div class="st-trade-plan__title">APEX: FĂRĂ INTRARE</div>
            <div class="st-trade-plan__reason">${escapeHtml(payload.block_reason || 'no data')}</div>
            <div class="st-trade-plan__row"><span>RVOL</span><b>${escapeHtml(fmtRvol(payload.rvol))}</b></div>
            <div class="st-trade-plan__row"><span>SPREAD</span><b>${escapeHtml(fmtNum(payload.spread_bps, 0))} bps</b></div>
            <div class="st-trade-plan__row"><span>CATALIST</span><b>${escapeHtml(payload.catalyst == null ? '—' : fmtNum(payload.catalyst, 0))}</b></div>`;
    }
}

async function refresh() {
    const symbol = activeSymbol();
    if (!symbol) {
        clearLines();
        renderPanel(null);
        return;
    }
    const timeframe = activeTf();
    try {
        const payload = await invoke('get_trade_plan', { symbol, timeframe });
        clearLines();
        if (payload.has_plan) drawLines(payload);
        renderPanel(payload);
    } catch (err) {
        console.warn('[trade-plan] refresh failed:', err);
        clearLines();
        renderPanel(null);
    }
}

export function initTradePlan() {
    // Chart rebuilds destroy the series (and our price lines with it) —
    // drop stale handles and re-evaluate on every load / TF switch.
    on('chart-loaded', () => { _lines.length = 0; refresh(); });
    on('tf-changed', () => { _lines.length = 0; refresh(); });
    refresh();
    _intervals.push(setInterval(refresh, POLL_MS));
}

export function disposeTradePlan() {
    for (const id of _intervals.splice(0)) clearInterval(id);
    clearLines();
}
