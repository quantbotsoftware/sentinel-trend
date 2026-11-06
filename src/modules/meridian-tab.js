// MERIDIAN tab — the operator's view of the A+B strategy on SPY/QQQ:
// per-symbol rule inputs (noise bands, VWAP, IBS, SMA200), today's
// decision schedule and the list of intents with the rule that fired.
// Data: `meridian-state` payload (see src-tauri/src/meridian/shadow.rs).

import { escapeHtml } from './esc.js';
import { fmtMoney, fmtNum } from './fmt.js';
import { invoke, listen } from './tauri-bridge.js';

const SCHEDULE = [
    ['09:30', 'B · sell last night\'s carry at the open (if any)'],
    ['10:00 → 15:30', 'A · every 30 min: close above upper band → LONG · below lower band → SHORT · back through VWAP → EXIT'],
    ['15:59', 'A · flat · B · buy the carry if IBS is low and price is above SMA200'],
];

function pct(a, b) {
    if (!Number.isFinite(a) || !Number.isFinite(b) || b === 0) return '—';
    const p = (a / b - 1) * 100;
    return `${p > 0 ? '+' : ''}${fmtNum(p, 2)}%`;
}

function aState(v) {
    if (!Number.isFinite(v.last) || !Number.isFinite(v.upper)) return { text: 'waiting for bars', cls: '' };
    if (v.net_shares > 0) return { text: `LONG ${Math.round(v.net_shares)} · exit if close < VWAP ${fmtNum(v.vwap, 2)}`, cls: 'st-mrd-up' };
    if (v.net_shares < 0) return { text: `SHORT ${Math.abs(Math.round(v.net_shares))} · exit if close > VWAP ${fmtNum(v.vwap, 2)}`, cls: 'st-mrd-down' };
    if (v.last > v.upper) return { text: 'above upper band → LONG at the next :00/:30 close', cls: 'st-mrd-up' };
    if (v.last < v.lower) return { text: 'below lower band → SHORT at the next :00/:30 close', cls: 'st-mrd-down' };
    return { text: 'inside the noise — no trade', cls: '' };
}

function bState(v) {
    if (!Number.isFinite(v.ibs)) return 'waiting for bars';
    const trend = v.above_sma === true ? 'above SMA200 ✓' : v.above_sma === false ? 'below SMA200 ✗' : 'SMA200 n/a';
    const ibs = fmtNum(v.ibs, 2);
    if (v.above_sma === false) return `IBS ${ibs} · ${trend} → no carry`;
    if (v.ibs <= 0.3) return `IBS ${ibs} (closing low) · ${trend} → FULL carry at 15:59`;
    if (v.ibs < 0.7) return `IBS ${ibs} · ${trend} → HALF carry at 15:59`;
    return `IBS ${ibs} (closing high) · ${trend} → no carry`;
}

function rowHtml(v) {
    const a = aState(v);
    return `<tr>
        <td class="st-mrd-sym">${escapeHtml(v.symbol)}</td>
        <td>${fmtNum(v.last, 2)} <span class="st-mrd-dim">${pct(v.last, v.prev_close)}</span></td>
        <td class="st-mrd-up">${fmtNum(v.upper, 2)}</td>
        <td class="st-mrd-down">${fmtNum(v.lower, 2)}</td>
        <td>${fmtNum(v.vwap, 2)}</td>
        <td class="${a.cls}">${escapeHtml(a.text)}</td>
        <td>${escapeHtml(bState(v))}</td>
        <td>${Number.isFinite(v.sigma_cc) ? `${fmtNum(v.sigma_cc * 100, 2)}%` : '—'}</td>
    </tr>`;
}

function intentHtml(i, execute) {
    const verb = i.delta_shares > 0 ? 'BUY' : 'SELL';
    const cls = i.delta_shares > 0 ? 'st-mrd-up' : 'st-mrd-down';
    return `<tr>
        <td>${escapeHtml(i.time_et || '')}</td>
        <td class="st-mrd-sym">${escapeHtml(i.symbol)}</td>
        <td class="${cls}">${verb} ${Math.abs(Math.round(i.delta_shares))}</td>
        <td>${fmtNum(i.fill_price, 2)}</td>
        <td>${Math.round(i.net_after)}</td>
        <td class="st-mrd-dim">${escapeHtml(i.label || '')}${execute ? '' : ' · shadow'}</td>
    </tr>`;
}

export function render(payload) {
    const snap = payload?.snapshot || {};
    const views = Array.isArray(snap.views) ? snap.views : [];
    const body = document.getElementById('st-mrd-symbols');
    if (body) body.innerHTML = views.map(rowHtml).join('') || '<tr><td colspan="8" class="st-mrd-dim">syncing SPY/QQQ history…</td></tr>';
    const intents = Array.isArray(snap.intents_today) ? snap.intents_today : [];
    const ib = document.getElementById('st-mrd-intents');
    if (ib) ib.innerHTML = intents.map((i) => intentHtml(i, payload.execute)).join('') || '<tr><td colspan="6" class="st-mrd-dim">no decision yet today</td></tr>';
    const eq = document.getElementById('st-mrd-equity');
    if (eq) eq.textContent = `engine replay over ${Object.values(snap.sessions || {})[0] || 0} cached sessions: ${fmtMoney(Number(snap.equity) || 0)} from $100K (simulated, not the account) · costs ${fmtMoney(Number(snap.total_cost) || 0)}`;
    const planned = Array.isArray(payload?.planned) ? payload.planned : [];
    const applied = Array.isArray(payload?.applied) ? payload.applied : [];
    const ob = document.getElementById('st-mrd-orders');
    if (ob) {
        const rows = planned.flatMap(([sym, acts]) => acts.map((a) => {
            if (a.Submit) {
                const s = a.Submit;
                const cls = s.qty > 0 ? 'st-mrd-up' : 'st-mrd-down';
                return `<tr><td class="st-mrd-sym">${escapeHtml(sym)}</td><td class="${cls}">${s.qty > 0 ? 'BUY' : 'SELL'} ${Math.abs(Math.round(s.qty))}</td><td>${escapeHtml(String(s.tif))}</td><td class="st-mrd-dim">${escapeHtml(s.client_order_id)}</td></tr>`;
            }
            if (a.Cancel) return `<tr><td class="st-mrd-sym">${escapeHtml(sym)}</td><td>CANCEL</td><td></td><td class="st-mrd-dim">${escapeHtml(a.Cancel.client_order_id)}</td></tr>`;
            return '';
        }));
        ob.innerHTML = rows.join('') || `<tr><td colspan="4" class="st-mrd-dim">${payload?.execute ? 'position matches the plan — nothing to send' : 'dry-run: nothing planned at the last poll'}</td></tr>`;
    }
    const ap = document.getElementById('st-mrd-applied');
    if (ap) ap.textContent = applied.length ? applied.join(' · ') : '';
    const mode = document.getElementById('st-mrd-mode');
    if (mode) mode.textContent = payload?.execute ? 'EXECUTE — orders are sent' : 'SHADOW — decisions logged, no orders (twin test)';
    const sched = document.getElementById('st-mrd-schedule');
    if (sched && !sched.childElementCount) {
        sched.innerHTML = SCHEDULE.map(([t, d]) => `<tr><td class="st-mrd-sym">${t}</td><td>${escapeHtml(d)}</td></tr>`).join('');
    }
}

export async function initMeridianTab() {
    render({ snapshot: {}, execute: false });
    listen('meridian-state', (event) => render(event.payload));
    // The driver may have replayed before the UI loaded (or after the poll
    // window closed): pull the last state instead of waiting for the next one.
    try {
        const last = await invoke('get_meridian_state');
        if (last) render({ ...last, new_intents: [] });
    } catch (_) { /* driver not started yet */ }
}
