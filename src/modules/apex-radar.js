// APEX radar sidebar (2026-07-04) — "where do I trade, on which TF?".
//
// Polls `get_apex_radar`, which sweeps EVERY watchlist symbol (the
// same authoritative list the bot trades from) across the execution
// timeframes (5m + 1m) through the identical APEX gate chain. Renders:
//   * valid plans (best ignition score first) — click loads the
//     symbol + timeframe on the chart, where the trade-plan overlay
//     draws the ENTRY/STOP/TARGET lines;
//   * "hot" names that pass liquidity + evidence but have no trigger
//     bar yet — watch closely, do not enter early.

import { invoke } from './tauri-bridge.js';
import { switchSymbol } from './discovery/index.js';
import { escapeHtml, escapeAttr } from './esc.js';
import { fmtNum, fmtRvol } from './fmt.js';

const $id = (id) => document.getElementById(id);

const POLL_MS = 30_000;
const _intervals = [];

function selectTf(tf) {
    const safe = tf === '1m' || tf === '15m' ? tf : '5m';
    const btn = document.querySelector(`.st-tf-btn[data-tf="${safe}"]`);
    if (btn && !btn.classList.contains('st-tf-btn--active')) btn.click();
}

function planRow(p) {
    const side = p.side === 'long' ? '▲ LONG' : '▼ SHORT';
    const sideCls = p.side === 'long' ? 'st-apex-radar__long' : 'st-apex-radar__short';
    const tip =
        `${p.symbol} ${p.timeframe} — plan APEX valid\n` +
        `ENTRY $${fmtNum(p.entry)} / STOP $${fmtNum(p.stop)} / TARGET $${fmtNum(p.target)}\n` +
        `${p.shares} acțiuni · risc $${fmtNum(p.risk_dollars, 0)} · R:R 1:${fmtNum(p.rr, 1)} · scor ${fmtNum(p.score, 0)}`;
    return `<div class="st-apex-radar__row st-apex-radar__row--plan" data-symbol="${escapeAttr(p.symbol)}" data-tf="${escapeAttr(p.timeframe)}" title="${escapeAttr(tip)}">
        <b>${escapeHtml(p.symbol)}</b>
        <span class="st-apex-radar__tf">${escapeHtml(p.timeframe)}</span>
        <span class="${sideCls}">${escapeHtml(side)}</span>
        <span class="st-apex-radar__score">${escapeHtml(fmtNum(p.score, 0))}</span>
    </div>`;
}

function nearRow(p) {
    const tip = `${p.symbol} ${p.timeframe} — fierbinte, fără trigger încă\n${p.block_reason || ''}\nRVOL ${fmtRvol(p.rvol)} · $${fmtNum(p.price)}`;
    return `<div class="st-apex-radar__row st-apex-radar__row--near" data-symbol="${escapeAttr(p.symbol)}" data-tf="${escapeAttr(p.timeframe)}" title="${escapeAttr(tip)}">
        <b>${escapeHtml(p.symbol)}</b>
        <span class="st-apex-radar__tf">${escapeHtml(p.timeframe)}</span>
        <span class="st-apex-radar__rvol">${escapeHtml(fmtRvol(p.rvol))}</span>
    </div>`;
}

function render(payload) {
    const el = $id('st-apex-radar');
    if (!el) return;
    if (!payload) {
        el.textContent = '---';
        return;
    }
    const plans = payload.plans || [];
    const near = payload.near || [];
    if (plans.length === 0 && near.length === 0) {
        el.innerHTML = `<div class="st-apex-radar__empty">0 setup-uri (${escapeHtml(String(payload.scanned))} scanate)</div>`;
        return;
    }
    let html = '';
    if (plans.length > 0) {
        html += `<div class="st-apex-radar__head st-apex-radar__head--go">INTRĂ ACUM</div>`;
        html += plans.map(planRow).join('');
    }
    if (near.length > 0) {
        html += `<div class="st-apex-radar__head">FIERBINTE — FĂRĂ TRIGGER</div>`;
        html += near.map(nearRow).join('');
    }
    el.innerHTML = html;

    el.querySelectorAll('.st-apex-radar__row').forEach((row) => {
        row.addEventListener('click', () => {
            const sym = row.dataset.symbol;
            const tf = row.dataset.tf;
            if (!sym) return;
            selectTf(tf);
            switchSymbol(sym);
        });
    });
}

async function refresh() {
    try {
        const payload = await invoke('get_apex_radar');
        render(payload);
    } catch (err) {
        console.warn('[apex-radar] refresh failed:', err);
        render(null);
    }
}

export function initApexRadar() {
    refresh();
    _intervals.push(setInterval(refresh, POLL_MS));
}

export function disposeApexRadar() {
    for (const id of _intervals.splice(0)) clearInterval(id);
}
