import { escapeHtml, escapeAttr } from '../esc.js';
import { fmtNum, fmtRvol, rvolIsCapped } from '../fmt.js';
import {
    derivePillsFromFlags,
    pillClassFor,
    renderCatalystPills,
} from '../catalyst-pills.js';
import { openOrderForm } from '../manual-order.js';
import { $id, disc, switchSymbol } from './index.js';
import { wireNewsPillClicks } from './news-modal.js';
import { filterTodayRows } from './session-today.js';

// Re-export for watchlist.js (kept for stable import surface).
export { renderCatalystPills, pillClassFor };

let _newsWired = false;

// ─── Scan Results Table ─────────────────────────────────────────────

export function renderScanResults(results) {
    disc.scanResults = filterTodayRows(results || []);
    const tbody = $id('st-scan-tbody');
    const empty = $id('st-scan-empty');
    if (!tbody) return;
    if (!_newsWired) {
        wireNewsPillClicks(tbody, (sym) => {
            const row = disc.scanResults.find((r) => r.symbol === sym);
            if (!row) return null;
            return { headline: row.news_headline, url: row.news_url };
        });
        _newsWired = true;
    }
    const label = $id('st-scan-results-label');
    if (label) label.textContent = disc.scanResults.length > 0 ? `SCAN RESULTS (${disc.scanResults.length})` : 'SCAN RESULTS';
    if (disc.scanResults.length === 0) {
        tbody.innerHTML = '';
        if (empty) {
            empty.style.display = '';
            empty.textContent = 'No today movers yet — run SCAN in session (04:05–15:30 ET)';
        }
        return;
    }
    if (empty) empty.style.display = 'none';
    const sorted = sortScanArray(disc.scanResults);
    tbody.innerHTML = sorted.map((r) => {
        const chgCls = r.change_percent >= 0 ? 'st-scan-chg--up' : 'st-scan-chg--down';
        const chgSign = r.change_percent >= 0 ? '+' : '';
        const sym = escapeHtml(r.symbol);
        const isStale = !!r.is_stale;
        const rowCls = isStale ? 'st-stale-row' : '';
        const pillsHtml = renderCatalystPills(derivePillsFromFlags(r.catalyst_flags), { symbol: r.symbol });
        const headline = String(r.news_headline || '').trim();
        const headlineHtml = headline
            ? `<div class="st-scan-headline" title="${escapeAttr(headline)}">${escapeHtml(headline)}</div>`
            : '';
        const spreadCls = spreadCellClass(r.spread_bps);
        const ageCls = ageCellClass(r.last_trade_age_seconds, isStale);
        const rvolAttr = rvolIsCapped(r.rvol)
            ? ` title="Actual: ${escapeHtml(String(r.rvol))}x (capped at 999x+ for readability)"`
            : '';
        return `<tr data-symbol="${sym}" class="${rowCls}">
            <td class="st-scan-sym">${sym}${headlineHtml}</td>
            <td class="${pillsHtml ? 'st-catalyst-cell' : 'st-catalyst-cell st-catalyst-cell--empty'}">${pillsHtml || '\u2014'}</td>
            <td>$${fmtNum(r.price)}</td>
            <td class="${chgCls}">${chgSign}${fmtNum(r.change_percent)}%</td>
            <td>${formatVol(r.volume)}</td>
            <td${rvolAttr}>${fmtRvol(r.rvol)}</td>
            <td>$${fmtNum(r.bid)}</td>
            <td>$${fmtNum(r.ask)}</td>
            <td class="${spreadCls}">${fmtNum(r.spread_bps)}bps</td>
            <td class="${ageCls}">${formatAge(r.last_trade_age_seconds)}</td>
            <td><button class="st-scan-btn-buy" data-symbol="${sym}">BUY</button></td>
        </tr>`;
    }).join('');

    tbody.querySelectorAll('tr').forEach((row) => {
        row.addEventListener('click', (e) => {
            if (e.target.classList.contains('st-scan-btn-buy')) return;
            if (e.target.closest('[data-news-symbol]')) return;
            tbody.querySelectorAll('.st-scan-row--active').forEach((r) => r.classList.remove('st-scan-row--active'));
            row.classList.add('st-scan-row--active');
            const sym = row.dataset.symbol;
            if (sym) switchSymbol(sym);
        });
    });

    tbody.querySelectorAll('.st-scan-btn-buy').forEach((btn) => {
        btn.addEventListener('click', (e) => {
            e.stopPropagation();
            const sym = btn.dataset.symbol;
            if (sym) {
                switchSymbol(sym);
                openOrderForm(sym, disc.livePrices[sym]?.price || 0);
            }
        });
    });
}

export function sortScanArray(arr) {
    const dir = disc.scanSortAsc ? 1 : -1;
    return [...arr].sort((a, b) => {
        if (disc.scanSortCol === 'symbol') {
            return a.symbol < b.symbol ? -dir : a.symbol > b.symbol ? dir : 0;
        }
        const va = a[disc.scanSortCol] ?? 0;
        const vb = b[disc.scanSortCol] ?? 0;
        return (va - vb) * dir;
    });
}

export function updateSortArrows() {
    document.querySelectorAll('#st-scan-table thead th[data-sort]').forEach((th) => {
        const col = th.getAttribute('data-sort');
        const arrow = th.querySelector('.st-scan-header-sort');
        if (col !== disc.scanSortCol) { if (arrow) arrow.remove(); return; }
        const el = arrow || Object.assign(document.createElement('span'), { className: 'st-scan-header-sort' });
        el.textContent = disc.scanSortAsc ? '\u25B2' : '\u25BC';
        if (!arrow) th.appendChild(el);
    });
}

export function setupScanTableSort() {
    document.querySelectorAll('#st-scan-table thead th[data-sort]').forEach((th) => {
        th.addEventListener('click', () => {
            const col = th.getAttribute('data-sort');
            if (disc.scanSortCol === col) { disc.scanSortAsc = !disc.scanSortAsc; }
            else { disc.scanSortCol = col; disc.scanSortAsc = col === 'symbol'; }
            try { localStorage.setItem('st-scan-sort', JSON.stringify({ col: disc.scanSortCol, asc: disc.scanSortAsc })); } catch { /* noop */ }
            updateSortArrows();
            renderScanResults(disc.scanResults);
        });
    });
    updateSortArrows();
}

// ─── Helpers ────────────────────────────────────────────────────────

export function formatVol(val) {
    if (val >= 1_000_000) return (val / 1_000_000).toFixed(1) + 'M';
    if (val >= 1_000) return (val / 1_000).toFixed(1) + 'K';
    return Math.round(val).toString();
}

// ─── S14 / S15.5: spread + age coloring ─────────────────────────────
//
// (Pills moved to `../catalyst-pills.js` in S15.5 — single source of
// truth for `catalyst_flags → pill labels`.)

/** < 10 bps  → tight (green); >= 50 bps → wide (red); else neutral. */
export function spreadCellClass(bps) {
    if (!Number.isFinite(bps) || bps <= 0) return 'st-spread';
    if (bps < 10) return 'st-spread st-spread--tight';
    if (bps >= 50) return 'st-spread st-spread--wide';
    return 'st-spread';
}

/** < 5s → fresh (green); >= 30s → warn (orange). Stale rows fall through. */
export function ageCellClass(ageSec, isStale) {
    if (isStale) return 'st-age st-age--warn';
    const a = Number(ageSec);
    if (!Number.isFinite(a) || a < 0) return 'st-age';
    if (a < 5) return 'st-age st-age--fresh';
    if (a >= 30) return 'st-age st-age--warn';
    return 'st-age';
}

/** Compact human-readable trade age. `0` ⇒ '<1s'. */
export function formatAge(ageSec) {
    const a = Number(ageSec);
    if (!Number.isFinite(a) || a < 0) return '\u2014';
    if (a === 0) return '<1s';
    if (a < 60) return `${a}s`;
    return `${Math.floor(a / 60)}m`;
}
