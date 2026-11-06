// PRIME tab — conviction engine console (F5c).
//
// Three read-only views over the PRIME backend:
//   1. Conviction heat-line — every arbitrated decision of the last
//      24h as a colored cell (green = entered, red = vetoed), height
//      scaled by conviction. The at-a-glance answer to "how hot is
//      the engine running today?".
//   2. Veto ledger — grouped (stage, reason) counts from the decision
//      journal.
//   3. Shadow book — counterfactual R-multiples of the vetoed trades.
//
// All rendering goes through escapeHtml (S07); listeners auto-tracked
// by tauri-bridge (F1); no polling — refresh on tab activation and on
// bot events while the tab is visible.

import { on } from './bus.js';
import { escapeHtml, escapeAttr } from './esc.js';
import { fmtNum } from './fmt.js';
import { invoke, listen } from './tauri-bridge.js';

const $id = (id) => document.getElementById(id);

function isTabVisible() {
    const tab = $id('st-tab-prime');
    return tab && !tab.classList.contains('st-tab-content--hidden');
}

// ─── Heat-line + decision feed ───────────────────────────────────────

function convClass(c) {
    if (c == null) return 'st-prime-cell--nocv';
    if (c >= 0.75) return 'st-prime-cell--high';
    if (c >= 0.55) return 'st-prime-cell--mid';
    return 'st-prime-cell--low';
}

function renderHeatLine(decisions) {
    const el = $id('st-prime-heat');
    const empty = $id('st-prime-heat-empty');
    if (!el) return;
    if (!decisions.length) {
        el.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';
    // Oldest → newest, left → right (chronological reading order).
    const cells = [...decisions].reverse().map((d) => {
        const entered = d.outcome === 'entered';
        const cv = d.conviction;
        const h = cv != null ? Math.max(20, Math.round(cv * 100)) : 15;
        const title = `${escapeAttr(d.symbol)} ${escapeAttr(d.source)} ${escapeAttr(d.direction)}`
            + ` — ${escapeAttr(d.outcome)}`
            + (cv != null ? ` (conv ${(cv * 100).toFixed(0)}%)` : '')
            + (d.veto_reason ? ` — ${escapeAttr(d.veto_reason)}` : '');
        const cls = entered ? 'st-prime-cell--enter' : 'st-prime-cell--veto';
        return `<div class="st-prime-cell ${cls} ${convClass(cv)}"
                     style="height:${h}%" title="${title}"></div>`;
    });
    el.innerHTML = cells.join('');
}

function fmtTime(ts) {
    const d = new Date(ts * 1000);
    const hh = String(d.getHours()).padStart(2, '0');
    const mm = String(d.getMinutes()).padStart(2, '0');
    return `${hh}:${mm}`;
}

function renderDecisionFeed(decisions) {
    const body = $id('st-prime-dec-body');
    const empty = $id('st-prime-dec-empty');
    if (!body) return;
    if (!decisions.length) {
        body.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';
    body.innerHTML = decisions.slice(0, 80).map((d) => {
        const entered = d.outcome === 'entered';
        // P0 — deferred signal exits (winner handed to the PRIME ladder).
        const deferred = d.outcome === 'exit_deferred';
        const outcomeCls = entered ? 'st-prime-out--enter'
            : deferred ? 'st-prime-out--defer' : 'st-prime-out--veto';
        const outcomeLabel = entered ? 'ENTERED' : deferred ? 'EXIT DEFERRED' : 'VETOED';
        const dirCls = d.direction === 'long' ? 'st-pos-side--long'
            : d.direction === 'short' ? 'st-pos-side--short' : '';
        const conv = d.conviction != null ? `${(d.conviction * 100).toFixed(0)}%` : '—';
        const detail = entered
            ? `@ $${fmtNum(d.price, 2)}`
            : `${d.veto_stage || ''}: ${d.veto_reason || ''}`;
        return `<tr>
            <td>${fmtTime(d.ts)}</td>
            <td>${escapeHtml(d.symbol)}</td>
            <td>${escapeHtml(d.source)}</td>
            <td class="${dirCls}">${escapeHtml(d.direction.toUpperCase())}</td>
            <td>${fmtNum(d.raw_score, 1)}</td>
            <td>${conv}</td>
            <td class="${outcomeCls}">${outcomeLabel}</td>
            <td class="st-prime-detail">${escapeHtml(detail)}</td>
        </tr>`;
    }).join('');
}

// ─── Veto ledger ─────────────────────────────────────────────────────

function renderVetoLedger(rows) {
    const body = $id('st-prime-veto-body');
    const empty = $id('st-prime-veto-empty');
    if (!body) return;
    if (!rows.length) {
        body.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';
    body.innerHTML = rows.slice(0, 20).map((r) => `<tr>
        <td>${escapeHtml(r.veto_stage)}</td>
        <td class="st-prime-detail">${escapeHtml(r.veto_reason)}</td>
        <td class="st-prime-count">${r.count}</td>
    </tr>`).join('');
}

// ─── Shadow book ─────────────────────────────────────────────────────

function renderShadowBook(payload) {
    const body = $id('st-prime-shadow-body');
    const empty = $id('st-prime-shadow-empty');
    if (!body) return;
    const rows = payload?.summary || [];
    if (!rows.length) {
        body.innerHTML = '';
        if (empty) {
            empty.style.display = 'block';
            const open = payload?.open_count || 0;
            empty.textContent = open > 0
                ? `${open} shadow trade(s) open — awaiting resolution`
                : 'No resolved shadows yet';
        }
        return;
    }
    if (empty) empty.style.display = 'none';
    body.innerHTML = rows.slice(0, 20).map((r) => {
        const rCls = r.total_r >= 0 ? 'st-prime-r--missed' : 'st-prime-r--saved';
        const sign = r.total_r >= 0 ? '+' : '';
        return `<tr>
            <td>${escapeHtml(r.veto_stage)}</td>
            <td>${escapeHtml(r.source)}</td>
            <td class="st-prime-count">${r.resolved}</td>
            <td class="st-prime-count">${r.wins}</td>
            <td class="${rCls}">${sign}${fmtNum(r.total_r, 2)}R</td>
        </tr>`;
    }).join('');
}

// ─── Hospital (P1) ───────────────────────────────────────────────────

function renderHospital(payload) {
    const body = $id('st-prime-hosp-body');
    const symEl = $id('st-prime-hosp-symbols');
    const empty = $id('st-prime-hosp-empty');
    if (!body) return;
    const quarantined = payload?.quarantined || [];
    const banned = payload?.banned_symbols || [];
    if (!quarantined.length && !banned.length) {
        body.innerHTML = '';
        if (symEl) symEl.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';
    body.innerHTML = quarantined.map((q) => `<tr>
        <td class="st-prime-hosp-name">${escapeHtml(q.source)}</td>
        <td>${fmtTime(q.since_ts)}</td>
        <td class="st-prime-r--saved">${(q.reliability_at_admit * 100).toFixed(0)}%</td>
    </tr>`).join('');
    if (symEl) {
        symEl.innerHTML = banned.length
            ? `<span class="st-prime-hosp-label">SYMBOLS BANNED TODAY:</span> `
                + banned.map((s) => `<span class="st-prime-hosp-ban">${escapeHtml(s)}</span>`).join(' ')
            : '';
    }
}

// ─── Weights Lab (P3) ────────────────────────────────────────────────

const WEIGHT_LABELS = [
    ['w_signal', 'SIGNAL'],
    ['w_reliability', 'RELIABILITY'],
    ['w_regime', 'REGIME FIT'],
    ['w_catalyst', 'CATALYST'],
    ['w_rvol', 'RVOL'],
    ['w_spread', 'SPREAD (−)'],
    ['threshold', 'THRESHOLD'],
];

function renderWeightsLab(payload) {
    const body = $id('st-prime-lab-body');
    const evidence = $id('st-prime-lab-evidence');
    const applyBtn = $id('st-prime-lab-apply');
    const empty = $id('st-prime-lab-empty');
    if (!body) return;
    const cur = payload?.current;
    const sug = payload?.suggested;
    if (!cur) return;
    if (empty) empty.style.display = sug ? 'none' : 'block';
    if (applyBtn) applyBtn.style.display = sug ? 'inline-block' : 'none';
    body.innerHTML = WEIGHT_LABELS.map(([key, label]) => {
        const c = cur[key];
        const s = sug ? sug.weights[key] : null;
        const changed = s != null && Math.abs(s - c) > 1e-9;
        const sCls = changed ? 'st-prime-lab-changed' : '';
        return `<tr>
            <td>${escapeHtml(label)}</td>
            <td class="st-prime-count">${fmtNum(c, 3)}</td>
            <td class="st-prime-count ${sCls}">${s != null ? fmtNum(s, 3) : '—'}</td>
        </tr>`;
    }).join('');
    if (evidence) {
        evidence.innerHTML = sug
            ? `Sample <span class="st-prime-lab-num">${sug.sample}</span> decisions —`
                + ` baseline <span class="st-prime-lab-num">${fmtNum(sug.baseline_r, 1)}R</span>`
                + ` (${sug.baseline_selected} sel) vs suggested`
                + ` <span class="st-prime-lab-num st-prime-lab-changed">${fmtNum(sug.suggested_r, 1)}R</span>`
                + ` (${sug.suggested_selected} sel)`
            : '';
    }
}

async function applySuggestedWeights() {
    try {
        await invoke('apply_suggested_weights');
        const payload = await invoke('get_prime_weights');
        renderWeightsLab(payload);
    } catch (err) {
        console.error('[prime] apply weights failed:', err);
    }
}

// ─── Slippage (P5) ───────────────────────────────────────────────────

function renderSlippage(payload) {
    const body = $id('st-prime-slip-body');
    const empty = $id('st-prime-slip-empty');
    if (!body) return;
    const rows = payload?.summary || [];
    if (!rows.length) {
        body.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';
    body.innerHTML = rows.slice(0, 15).map((r) => {
        const avgCls = r.avg_bps > 10 ? 'st-prime-r--missed' : 'st-prime-r--saved';
        return `<tr>
            <td>${escapeHtml(r.symbol)}</td>
            <td class="st-prime-count">${r.fills}</td>
            <td class="${avgCls}">${fmtNum(r.avg_bps, 1)}</td>
            <td class="st-prime-detail">${fmtNum(r.worst_bps, 1)}</td>
        </tr>`;
    }).join('');
}

// ─── Debrief (P6) ────────────────────────────────────────────────────

function renderDebrief(d) {
    const el = $id('st-prime-debrief');
    const empty = $id('st-prime-debrief-empty');
    if (!el) return;
    const lines = d?.narrative || [];
    const hasContent = lines.length && !(lines.length === 1 && d.trades === 0
        && !d.exits_deferred && !d.shadows_resolved && !d.quarantined?.length);
    if (!hasContent) {
        el.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';
    const pnlCls = d.total_pnl >= 0 ? 'st-prime-debrief-pnl--pos' : 'st-prime-debrief-pnl--neg';
    const header = `<div class="st-prime-debrief-head">
        <span class="st-prime-debrief-date">${escapeHtml(d.date)}</span>
        <span class="${pnlCls}">${d.total_pnl >= 0 ? '+' : ''}$${fmtNum(d.total_pnl, 2)}</span>
        <span class="st-prime-debrief-meta">${d.trades} trades — ${d.wins}W / ${d.losses}L</span>
    </div>`;
    const body = lines.map((l) => `<div class="st-prime-debrief-line">${escapeHtml(l)}</div>`).join('');
    el.innerHTML = header + body;
}

// ─── Loading ─────────────────────────────────────────────────────────
// (FRONTIER section is owned by frontier-panel.js — same bus event.)

async function loadPrime() {
    try {
        const [dec, veto, shadow, hospital, weights, slip, debrief] = await Promise.all([
            invoke('get_decisions', { symbol: '', fromTs: 0, toTs: 0 }),
            invoke('get_veto_summary', { hours: 24 }),
            invoke('get_shadow_summary'),
            invoke('get_hospital_status'),
            invoke('get_prime_weights'),
            invoke('get_slippage_ledger'),
            invoke('get_debrief'),
        ]);
        const decisions = dec?.decisions || [];
        renderHeatLine(decisions);
        renderDecisionFeed(decisions);
        renderVetoLedger(veto || []);
        renderShadowBook(shadow);
        renderHospital(hospital);
        renderWeightsLab(weights);
        renderSlippage(slip);
        renderDebrief(debrief);
    } catch (err) {
        console.error('[prime] load failed:', err);
        const empty = $id('st-prime-dec-empty');
        if (empty) {
            empty.style.display = 'block';
            empty.textContent = `PRIME load failed: ${err?.message || err}`;
        }
    }
}

// ─── Init ────────────────────────────────────────────────────────────

export function initPrime() {
    on('tab-prime-active', () => loadPrime());
    // Refresh when the bot acts, but only if the tab is on screen.
    listen('bot-order', () => { if (isTabVisible()) loadPrime(); });
    listen('bot-blocked', () => { if (isTabVisible()) loadPrime(); });
    const applyBtn = $id('st-prime-lab-apply');
    if (applyBtn) applyBtn.addEventListener('click', applySuggestedWeights);
}
