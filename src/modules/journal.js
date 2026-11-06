import { on } from './bus.js';
import { escapeHtml } from './esc.js';
import { showToast } from './toast.js';

import { invoke, listen } from './tauri-bridge.js';

const $id = (id) => document.getElementById(id);

let allEntries = [];
let currentFilter = { symbol: 'all', period: 'all', pnl: 'all' };
let sortCol = 'entry_time';
let sortAsc = false;

// ─── Init ────────────────────────────────────────────────────────────

export async function initJournal() {
    setupFilters();
    setupExport();
    setupLiveUpdate();
    on('tab-journal-active', () => loadJournal());
    // S16.4: load once at startup so the tab shows data even if the user
    // is already on JOURNAL when the app launches (the bus event only
    // fires on tab clicks, leaving the table empty until then).
    loadJournal();
}

// ─── Data Loading ────────────────────────────────────────────────────

async function loadJournal() {
    const empty = $id('st-jrn-empty');
    try {
        if (empty) {
            empty.style.display = 'block';
            empty.textContent = 'Loading journal...';
            empty.style.color = '';
        }
        const resp = await invoke('get_journal');
        allEntries = resp.entries || [];
        renderStats(resp.stats);
        // v2.5.1 HF — surface the Alpaca-source equity figures
        // alongside the realized P/L sum. See AccountSummary docstring
        // in commands/journal.rs for the live 2026-05-18 incident
        // that motivated this dual-metric display.
        renderAccount(resp.account);
        renderDailyPnl(resp.daily_pnl || []);
        renderTable();
        // S16.4 diagnostic: keep stale "No trades recorded" from showing
        // when get_journal succeeded with entries; renderTable handles
        // the empty-state visibility correctly when entries.length > 0.
        if (empty && allEntries.length === 0) {
            empty.textContent = 'No trades recorded yet';
        }
    } catch (err) {
        console.error('Journal load failed:', err);
        if (empty) {
            empty.style.display = 'block';
            empty.style.color = '#ff6b6b';
            empty.textContent = `Journal load failed: ${err?.message || err}`;
        }
    }
}

// ─── Stats Row ───────────────────────────────────────────────────────

function renderStats(s) {
    if (!s) return;
    setText('st-jrn-trades', String(s.closed_trades));
    setText('st-jrn-winrate', s.win_rate == null ? '—' : `${s.win_rate.toFixed(1)}%`);
    setText('st-jrn-avg-win', fmtPnl(s.avg_win));
    setText('st-jrn-avg-loss', fmtPnl(s.avg_loss));
    setText('st-jrn-pf', s.profit_factor == null ? '—' : s.profit_factor === Infinity ? '∞' : s.profit_factor.toFixed(2));
    setText('st-jrn-total-pnl', fmtPnl(s.total_pnl));
    setText('st-jrn-maxdd', fmtPnl(-s.max_drawdown));
    setText('st-jrn-best', fmtPnl(s.best_trade));
    setText('st-jrn-worst', fmtPnl(s.worst_trade));

    setPnlColor('st-jrn-total-pnl', s.total_pnl);
    setPnlColor('st-jrn-avg-win', s.avg_win);
    setPnlColor('st-jrn-avg-loss', s.avg_loss);
    setPnlColor('st-jrn-best', s.best_trade);
    setPnlColor('st-jrn-worst', s.worst_trade);

    const streakEl = $id('st-jrn-streak');
    if (streakEl) {
        const streak = s.current_streak;
        const abs = Math.abs(streak);
        const label = streak > 0 ? `${abs}W` : streak < 0 ? `${abs}L` : '—';
        streakEl.textContent = label;
        streakEl.className = 'hud-value hud-value--mono';
        if (streak > 0) streakEl.classList.add('hud-value--green');
        else if (streak < 0) streakEl.classList.add('hud-value--red');
    }
}

// v2.5.1 HF — Account-equity row. The "Daily Δ" pill is the Alpaca
// "Daily Change" figure (equity_now − equity_open_today); "Unreal" is
// the live mark-to-market on open positions. Together with the
// existing "Realized" cell (renamed from "PnL" in the same patch)
// these three answer the question "did I make money today and where
// is it sitting?" — the realized cell alone is misleading when
// positions are still open or were carried overnight.
function renderAccount(acct) {
    if (!acct) return;
    setText('st-jrn-daily-change', fmtPnl(acct.daily_equity_change));
    setText('st-jrn-unrealized', fmtPnl(acct.unrealized_total));
    setPnlColor('st-jrn-daily-change', acct.daily_equity_change);
    setPnlColor('st-jrn-unrealized', acct.unrealized_total);
}

function renderDailyPnl(days) {
    const container = $id('st-jrn-daily');
    if (!container || days.length === 0) {
        if (container) container.innerHTML = '';
        return;
    }

    const recent = days.slice(-7);
    container.innerHTML = recent
        .map((d) => {
            const cls = d.pnl >= 0 ? 'st-jrn-daily-pos' : 'st-jrn-daily-neg';
            return `<div class="st-jrn-daily-row">
                <span class="st-jrn-daily-date">${escapeHtml(d.date.slice(5))}</span>
                <span class="${cls}">${fmtPnl(d.pnl)}</span>
                <span class="st-jrn-daily-count">${d.wins}/${d.trades}</span>
            </div>`;
        })
        .join('');
}

// ─── Table ───────────────────────────────────────────────────────────

function renderTable() {
    const tbody = $id('st-jrn-body');
    const empty = $id('st-jrn-empty');
    if (!tbody) return;

    const filtered = filterEntries(allEntries);
    const sorted = sortEntries(filtered);

    if (sorted.length === 0) {
        tbody.innerHTML = '';
        if (empty) empty.style.display = 'block';
        return;
    }
    if (empty) empty.style.display = 'none';

    tbody.innerHTML = sorted
        .map((e) => {
            const closed = e.exit_time != null;
            const pnl = e.pnl ?? 0;
            const pnlPct = closed && e.entry_price > 0
                ? ((pnl / (e.entry_price * e.qty)) * 100)
                : 0;
            const holdSecs = closed ? (e.exit_time - e.entry_time) : 0;
            const pnlCls = pnl >= 0 ? 'st-jrn-pnl-pos' : 'st-jrn-pnl-neg';
            const sideDir = e.side === 'Long' ? 'st-pos-side--long' : 'st-pos-side--short';
            // PRIME F2e — per-playbook attribution + fused conviction.
            const playbook = e.strategy_id || 'fsm';
            const conv = e.conviction != null ? `${(e.conviction * 100).toFixed(0)}%` : '—';

            return `<tr class="${closed ? '' : 'st-jrn-row-open'}">
                <td>${escapeHtml(e.symbol)}</td>
                <td class="${sideDir}">${e.side === 'Long' ? 'LONG' : 'SHORT'}</td>
                <td>$${e.entry_price.toFixed(2)}</td>
                <td>${closed ? `$${e.exit_price.toFixed(2)}` : '—'}</td>
                <td>${e.qty.toFixed(2)}</td>
                <td class="${pnlCls}">${closed ? fmtPnl(pnl) : '—'}</td>
                <td class="${pnlCls}">${closed ? `${pnlPct.toFixed(2)}%` : '—'}</td>
                <td>${closed ? fmtHold(holdSecs) : '—'}</td>
                <td>${escapeHtml(e.exit_reason || (closed ? '—' : 'OPEN'))}</td>
                <td>${escapeHtml(playbook)}</td>
                <td>${conv}</td>
                <td>${e.entry_score.toFixed(1)}</td>
                <td>${fmtDate(e.entry_time)}</td>
            </tr>`;
        })
        .join('');
}

// ─── Filters ─────────────────────────────────────────────────────────

function setupFilters() {
    const symbolEl = $id('st-jrn-filter-symbol');
    const periodEl = $id('st-jrn-filter-period');
    const pnlEl = $id('st-jrn-filter-pnl');

    if (symbolEl) symbolEl.addEventListener('change', (e) => {
        currentFilter.symbol = e.target.value;
        renderTable();
    });
    if (periodEl) periodEl.addEventListener('change', (e) => {
        currentFilter.period = e.target.value;
        renderTable();
    });
    if (pnlEl) pnlEl.addEventListener('change', (e) => {
        currentFilter.pnl = e.target.value;
        renderTable();
    });

    const headers = document.querySelectorAll('#st-jrn-table thead th[data-sort]');
    headers.forEach((th) => {
        th.addEventListener('click', () => {
            const col = th.getAttribute('data-sort');
            if (sortCol === col) {
                sortAsc = !sortAsc;
            } else {
                sortCol = col;
                sortAsc = col === 'symbol';
            }
            renderTable();
        });
    });
}

function filterEntries(entries) {
    let filtered = entries;

    if (currentFilter.symbol !== 'all') {
        filtered = filtered.filter((e) => e.symbol === currentFilter.symbol);
    }

    if (currentFilter.period !== 'all') {
        const now = Date.now() / 1000;
        const cutoff = currentFilter.period === 'today'
            ? now - 86400
            : now - 7 * 86400;
        filtered = filtered.filter((e) => e.entry_time >= cutoff);
    }

    if (currentFilter.pnl === 'winners') {
        filtered = filtered.filter((e) => e.pnl != null && e.pnl >= 0);
    } else if (currentFilter.pnl === 'losers') {
        filtered = filtered.filter((e) => e.pnl != null && e.pnl < 0);
    }

    return filtered;
}

function sortEntries(entries) {
    const dir = sortAsc ? 1 : -1;
    return [...entries].sort((a, b) => {
        let va, vb;
        switch (sortCol) {
            case 'symbol': va = a.symbol; vb = b.symbol; return va < vb ? -dir : va > vb ? dir : 0;
            case 'playbook': {
                va = a.strategy_id || 'fsm'; vb = b.strategy_id || 'fsm';
                return va < vb ? -dir : va > vb ? dir : 0;
            }
            case 'conviction': va = a.conviction ?? -1; vb = b.conviction ?? -1; break;
            case 'pnl': va = a.pnl ?? 0; vb = b.pnl ?? 0; break;
            case 'entry_time': va = a.entry_time; vb = b.entry_time; break;
            case 'score': va = a.entry_score; vb = b.entry_score; break;
            case 'hold':
                va = (a.exit_time || 0) - a.entry_time;
                vb = (b.exit_time || 0) - b.entry_time;
                break;
            default: va = a.entry_time; vb = b.entry_time;
        }
        return (va - vb) * dir;
    });
}

// ─── Export CSV ───────────────────────────────────────────────────────

function setupExport() {
    const btn = $id('st-jrn-export');
    if (!btn) return;

    btn.addEventListener('click', async () => {
        try {
            const path = await invoke('export_journal_csv');
            showToast(`CSV exported: ${path}`, 'success');
        } catch (err) {
            showToast(`CSV export failed: ${err}`, 'error');
        }
    });
}

// ─── Live Update ─────────────────────────────────────────────────────

function setupLiveUpdate() {
    listen('bot-pnl', () => {
        const tab = $id('st-tab-journal');
        if (tab && !tab.classList.contains('st-tab-content--hidden')) {
            loadJournal();
        }
    });
}

// ─── Helpers ─────────────────────────────────────────────────────────

function setText(id, text) {
    const el = $id(id);
    if (el) el.textContent = text;
}

function setPnlColor(id, val) {
    const el = $id(id);
    if (!el) return;
    el.classList.remove('hud-value--green', 'hud-value--red');
    if (val > 0) el.classList.add('hud-value--green');
    else if (val < 0) el.classList.add('hud-value--red');
}

function fmtPnl(val) {
    if (val == null) return '—';
    const sign = val >= 0 ? '+' : '';
    return `${sign}$${val.toFixed(2)}`;
}

function fmtHold(secs) {
    if (secs <= 0) return '—';
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    if (m >= 60) {
        const h = Math.floor(m / 60);
        return `${h}h ${m % 60}m`;
    }
    return `${m}m ${s}s`;
}

function fmtDate(ts) {
    const d = new Date(ts * 1000);
    const month = String(d.getMonth() + 1).padStart(2, '0');
    const day = String(d.getDate()).padStart(2, '0');
    const hh = String(d.getHours()).padStart(2, '0');
    const mm = String(d.getMinutes()).padStart(2, '0');
    return `${month}/${day} ${hh}:${mm}`;
}

