// FRONTIER section of the PRIME tab (F4/F5).
//
// Three read-only views + one operator action:
//   1. Status line — mode, F5 go-live gate (PF / closes), shadow book.
//   2. Candidates/day strip — is the rate inside the 3–8 target band?
//   3. Kill-rate table — which condition kills the most candidates.
//   4. Calibration proposals — the engine PROPOSES threshold moves
//      with evidence; nothing changes until the operator clicks APPLY
//      (Weights Lab pattern).
//
// All rendering goes through escapeHtml (S07); refresh on PRIME tab
// activation (bus event shared with prime.js).

import { on } from './bus.js';
import { escapeHtml } from './esc.js';
import { fmtNum } from './fmt.js';
import { invoke } from './tauri-bridge.js';
import { showToast } from './toast.js';

const $id = (id) => document.getElementById(id);

let _proposals = [];

function renderStatus(status) {
    const el = $id('st-prime-frontier-status');
    if (!el || !status) return;
    const gateCls = status.frontier_gate_ok ? 'st-prime-gate--ok' : 'st-prime-gate--no';
    el.innerHTML =
        `<span class="st-prime-frontier-mode">${escapeHtml((status.frontier_mode || 'off').toUpperCase())}</span>`
        + `<span class="${gateCls}">${status.frontier_gate_ok ? 'SHADOW GATE OK' : 'SHADOW GATE NOT MET'}</span>`
        + `<span class="st-prime-frontier-kv">PF ${fmtNum(status.frontier_pf, 2)}</span>`
        + `<span class="st-prime-frontier-kv">closes ${status.frontier_closes || 0}/30</span>`
        + `<span class="st-prime-frontier-kv">shadow ${status.shadow_open || 0} open / ${status.shadow_resolved || 0} resolved</span>`;
}

function renderDays(report) {
    const el = $id('st-frontier-days');
    if (!el) return;
    const days = report?.per_day || [];
    if (!days.length) {
        el.innerHTML = '<span class="st-frontier-day-d">No shadow days recorded yet — target 3–8 candidates/day</span>';
        return;
    }
    const lo = report.target_lo ?? 3;
    const hi = report.target_hi ?? 8;
    el.innerHTML = days.slice(-7).map(([d, n]) => {
        const cls = n >= lo && n <= hi ? 'st-frontier-day-n--ok' : 'st-frontier-day-n--off';
        return `<div class="st-frontier-day">
            <span class="st-frontier-day-n ${cls}">${n}</span>
            <span class="st-frontier-day-d">${escapeHtml(String(d).slice(5))}</span>
        </div>`;
    }).join('')
    + `<div class="st-frontier-day">
        <span class="st-frontier-day-n">${fmtNum(report.candidates_per_day, 1)}</span>
        <span class="st-frontier-day-d">avg/day</span>
    </div>`;
}

function renderKillRate(kr) {
    const body = $id('st-prime-frontier-body');
    const empty = $id('st-prime-frontier-empty');
    if (!body) return;
    const rows = kr?.rows || [];
    if (!rows.length) {
        body.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';
    body.innerHTML = rows.slice(0, 12).map((r) => {
        const pctCls = r.kill_rate >= 0.5 ? 'st-prime-r--missed' : 'st-prime-r--saved';
        return `<tr>
            <td>${escapeHtml(r.setup)}</td>
            <td class="st-prime-detail">${escapeHtml(r.condition)}</td>
            <td class="st-prime-count">${r.total}</td>
            <td class="st-prime-count">${r.killed}</td>
            <td class="${pctCls}">${(r.kill_rate * 100).toFixed(0)}%</td>
        </tr>`;
    }).join('');
}

function renderCalibration(cal) {
    const body = $id('st-frontier-cal-body');
    const empty = $id('st-frontier-cal-empty');
    const applyBtn = $id('st-frontier-cal-apply');
    if (!body) return;
    _proposals = cal?.report?.proposals || [];
    if (applyBtn) applyBtn.style.display = _proposals.length ? 'inline-block' : 'none';
    if (!_proposals.length) {
        body.innerHTML = '';
        if (empty) {
            empty.style.display = 'block';
            empty.textContent = cal?.report?.in_target
                ? `In target band (${fmtNum(cal.report.candidates_per_day, 1)}/day) — no proposals`
                : 'Not enough shadow data for proposals yet';
        }
        return;
    }
    if (empty) empty.style.display = 'none';
    body.innerHTML = _proposals.map((p) => {
        const extra = p.projected_extra_per_day;
        const evid = extra >= 0
            ? `${p.sole_blocked} sole-blocked → +${fmtNum(extra, 1)}/day`
            : `keeps top candidates (trim ${fmtNum(-extra, 0)})`;
        return `<tr>
            <td class="st-prime-detail">${escapeHtml(p.setup)} · ${escapeHtml(p.condition)}</td>
            <td class="st-prime-count">${fmtNum(p.current, 2)}</td>
            <td class="st-prime-count st-prime-lab-changed">${fmtNum(p.suggested, 2)}</td>
            <td class="st-prime-detail">${escapeHtml(evid)}</td>
        </tr>`;
    }).join('');
}

async function applyProposals() {
    if (!_proposals.length) return;
    const applyBtn = $id('st-frontier-cal-apply');
    if (applyBtn) applyBtn.disabled = true;
    const overrides = {};
    for (const p of _proposals) overrides[p.key] = p.suggested;
    const n = _proposals.length;
    try {
        await invoke('apply_frontier_calibration', { overrides });
        showToast(`Applied ${n} Frontier threshold(s)`, 'success');
        await loadFrontierPanel();
    } catch (err) {
        showToast(`Apply failed: ${err}`, 'error');
    } finally {
        if (applyBtn) applyBtn.disabled = false;
    }
}

async function loadFrontierPanel() {
    const [status, kr, cal] = await Promise.all([
        invoke('get_frontier_status').catch(() => null),
        invoke('get_frontier_killrate', { hours: 24 }).catch(() => null),
        invoke('get_frontier_calibration', { days: 7 }).catch(() => null),
    ]);
    renderStatus(status);
    renderDays(cal?.report);
    renderKillRate(kr);
    renderCalibration(cal);
}

export function initFrontierPanel() {
    on('tab-prime-active', () => loadFrontierPanel());
    const applyBtn = $id('st-frontier-cal-apply');
    if (applyBtn) applyBtn.addEventListener('click', applyProposals);
}
