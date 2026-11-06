import { escapeHtml } from '../esc.js';
import { fmtNum, fmtRvol, rvolIsCapped } from '../fmt.js';
import { showToast, showToastWithAction } from '../toast.js';
import { invoke, listen } from '../tauri-bridge.js';
import { $id, disc, switchSymbol } from './index.js';
import { renderMovers } from './movers.js';
import { derivePillsFromFlags, renderCatalystPills } from '../catalyst-pills.js';
import { wireNewsPillClicks } from './news-modal.js';
import {
    ageCellClass,
    formatAge,
    formatVol,
    spreadCellClass,
} from './scan-results.js';
import {
    symbolBadge,
    isAutoUniverseEnabled,
    pinSymbol,
    unpinSymbol,
    promoteAutoToPinned,
    forceRemoveFromUniverse,
} from './auto-universe.js';

// ─── Watchlist Rendering ────────────────────────────────────────────

let _wlNewsWired = false;

export function renderWatchlist() {
    const tbody = $id('st-wl-tbody');
    if (!tbody) return;

    if (!_wlNewsWired) {
        wireNewsPillClicks(tbody);
        _wlNewsWired = true;
    }

    if (disc.watchlist.length === 0) {
        tbody.innerHTML = '<tr><td colspan="14" class="st-discovery-empty">Watchlist empty</td></tr>';
        return;
    }

    const auEnabled = isAutoUniverseEnabled();

    tbody.innerHTML = disc.watchlist
        .map((w) => {
            const live = disc.livePrices[w.symbol];
            const price = live ? live.price : w.price;
            const basePrice = w.price || price;
            const priceStr = price > 0 ? `$${fmtNum(price)}` : '---';
            const chg = w.change_percent;
            const arrowCls = chg >= 0 ? 'st-wl-arrow--up' : 'st-wl-arrow--down';
            const arrowChar = chg >= 0 ? '\u25B2' : '\u25BC';
            const chgCls = chg >= 0 ? 'st-scan-chg--up' : 'st-scan-chg--down';
            const chgSign = chg >= 0 ? '+' : '';
            const rvolCls = w.rvol > 2 ? 'st-scan-rvol-hot' : '';
            const rvolAttr = rvolIsCapped(w.rvol)
                ? ` title="Actual: ${escapeHtml(String(w.rvol))}x (capped at 999x+ for readability)"`
                : '';
            const scoreCls = w.st_score > 70 ? 'st-scan-score--high'
                : w.st_score >= 50 ? 'st-scan-score--mid' : 'st-scan-score--low';
            const sym = escapeHtml(w.symbol);
            const earningsMark = w.has_earnings ? '<span class="st-wl-earnings" title="Earnings today/tomorrow">E</span>' : '';
            const isStale = !!w.is_stale;
            const rowCls = isStale ? 'st-stale-row' : '';
            const pillsHtml = renderCatalystPills(derivePillsFromFlags(w.catalyst_flags), { symbol: w.symbol });
            const pillsCellCls = pillsHtml ? 'st-catalyst-cell' : 'st-catalyst-cell st-catalyst-cell--empty';
            const bidStr = w.bid > 0 ? `$${fmtNum(w.bid)}` : '\u2014';
            const askStr = w.ask > 0 ? `$${fmtNum(w.ask)}` : '\u2014';
            const spreadCls = spreadCellClass(w.spread_bps);
            const ageCls = ageCellClass(w.last_trade_age_seconds, isStale);

            // S18 — badge for AutoUniverse membership (PIN/AUTO).
            // When AutoUniverse is disabled, no badge is rendered so
            // the row matches legacy appearance exactly.
            const badge = auEnabled ? symbolBadge(w.symbol) : null;
            const badgeHtml = badge
                ? `<span class="st-au-badge st-au-badge--${badge.toLowerCase()}" title="S18 ${badge}">${badge}</span>`
                : '';

            // S18 — when AutoUniverse is enabled, the action button
            // depends on row type:
            //   PIN  → unpin (✕ keeps semantic continuity)
            //   AUTO → promote-to-pin (📌) primary; force-remove (✕) secondary
            //   neither → legacy add/remove from `cfg.ui.favorites`
            let actionHtml = '';
            if (auEnabled && badge === 'PIN') {
                actionHtml = `<button class="st-wl-remove" data-au="unpin" data-symbol="${sym}" title="Unpin">&times;</button>`;
            } else if (auEnabled && badge === 'AUTO') {
                actionHtml = `<button class="st-au-pinbtn" data-au="promote" data-symbol="${sym}" title="Pin (lock into universe)">📌</button>
                              <button class="st-wl-remove" data-au="force-remove" data-symbol="${sym}" title="Remove from universe">&times;</button>`;
            } else {
                actionHtml = `<button class="st-wl-remove" data-au="legacy" data-symbol="${sym}" title="Remove from watchlist">&times;</button>`;
            }

            return `<tr data-symbol="${sym}" data-base-price="${basePrice}" class="${rowCls}">
                <td><span class="st-wl-arrow ${arrowCls}">${arrowChar}</span></td>
                <td class="st-scan-sym">${badgeHtml}${sym}${earningsMark}</td>
                <td class="${pillsCellCls}">${pillsHtml || '\u2014'}</td>
                <td class="st-wl-price">${priceStr}</td>
                <td class="st-wl-chg ${chgCls}">${chgSign}${fmtNum(chg)}%</td>
                <td>${formatVol(w.volume)}</td>
                <td class="${rvolCls}"${rvolAttr}>${fmtRvol(w.rvol)}</td>
                <td>${fmtNum(w.gap_percent, 1)}%</td>
                <td>${bidStr}</td>
                <td>${askStr}</td>
                <td class="${spreadCls}">${fmtNum(w.spread_bps)}bps</td>
                <td class="${ageCls}">${formatAge(w.last_trade_age_seconds)}</td>
                <td class="${scoreCls}">${fmtNum(w.st_score, 1)}</td>
                <td>${actionHtml}</td>
            </tr>`;
        })
        .join('');

    tbody.querySelectorAll('tr').forEach((row) => {
        row.addEventListener('click', (e) => {
            if (e.target.closest('.st-wl-remove') || e.target.closest('.st-au-pinbtn')) return;
            if (e.target.closest('[data-news-symbol]')) return;
            const sym = row.dataset.symbol;
            if (sym) switchSymbol(sym);
        });
    });

    tbody.querySelectorAll('.st-wl-remove, .st-au-pinbtn').forEach((btn) => {
        btn.addEventListener('click', async (e) => {
            e.stopPropagation();
            const sym = btn.dataset.symbol;
            const action = btn.dataset.au;
            if (!sym) return;
            try {
                if (action === 'unpin') {
                    await unpinSymbol(sym);
                } else if (action === 'force-remove') {
                    await forceRemoveFromUniverse(sym);
                } else if (action === 'promote') {
                    await promoteAutoToPinned(sym);
                } else {
                    // Legacy mode — old favorites-based remove.
                    await invoke('remove_from_watchlist', { symbol: sym });
                    disc.watchlist = disc.watchlist.filter((w) => w.symbol !== sym);
                    renderWatchlist();
                    showToast(`${sym} removed`, 'success');
                }
            } catch (err) {
                showToast(`Action failed: ${err}`, 'error');
            }
        });
    });
}

// ─── Load Data ──────────────────────────────────────────────────────

export async function loadWatchlist() {
    try {
        disc.watchlist = await invoke('get_watchlist');
        renderWatchlist();
        renderSidebarWatchlist();
    } catch (err) {
        console.error('[discovery] get_watchlist failed:', err);
    }
}

// ─── Live Event Listeners ───────────────────────────────────────────

export function setupLiveListeners() {
    listen('discovery-movers', (event) => {
        disc.movers = event.payload || [];
        renderMovers(); // filters to today ET inside renderMovers
    });

    listen('price-update', (event) => {
        const { symbol, price } = event.payload;
        disc.livePrices[symbol] = { price, ts: Date.now() };
        updateWatchlistPrice(symbol, price);
    });

    listen('smarttrend-eval', (event) => {
        const p = event.payload;
        if (!p || !p.symbol) return;
        const item = disc.watchlist.find((w) => w.symbol === p.symbol);
        if (item && p.diagnostics) {
            item.st_score = p.diagnostics.combined_score || 0;
            renderWatchlist();
        }
    });

    listen('fmp-mover-propose', (event) => {
        const m = event.payload;
        if (!m || !m.symbol) return;
        const sign = m.change_pct >= 0 ? '+' : '';
        showToastWithAction(
            `${m.symbol} ${sign}${m.change_pct.toFixed(1)}% — Add to watchlist?`,
            'Add',
            async () => {
                await invoke('add_to_watchlist', { symbol: m.symbol });
                showToast(`${m.symbol} added`, 'success');
                await loadWatchlist();
            }
        );
    });

    listen('volume-spike', (event) => {
        const s = event.payload;
        if (!s) return;
        const sign = s.change_pct >= 0 ? '+' : '';
        showToast(`VOL SPIKE: ${s.symbol} ${sign}${s.change_pct.toFixed(1)}% ${formatVol(s.volume)}`, 'warning');
    });

    // S14: 30s watchlist refresh from `market::watchlist_refresh`. Merge
    // fresh metrics into `disc.watchlist` while preserving the canonical
    // order + Tier-A fields the backend already attached
    // (`has_earnings`, `st_score`, `st_state`).
    listen('watchlist-update', (event) => {
        const fresh = Array.isArray(event?.payload) ? event.payload : [];
        if (fresh.length === 0 || !Array.isArray(disc.watchlist) || disc.watchlist.length === 0) {
            return;
        }
        const idx = new Map(fresh.map((m) => [String(m.symbol).toUpperCase(), m]));
        disc.watchlist = disc.watchlist.map((w) => {
            const m = idx.get(String(w.symbol).toUpperCase());
            if (!m) return w;
            return {
                ...w,
                price: m.price,
                change_percent: m.change_percent,
                volume: m.volume,
                rvol: m.rvol,
                gap_percent: m.gap_percent,
                bid: m.bid,
                ask: m.ask,
                spread_bps: m.spread_bps,
                last_trade_age_seconds: m.last_trade_age_seconds,
                is_stale: m.is_stale,
                catalyst_flags: m.catalyst_flags,
                catalyst_pills: m.catalyst_pills,
            };
        });
        renderWatchlist();
    });
}

// ─── Manual Add ─────────────────────────────────────────────────────

export function setupManualAdd() {
    const input = $id('st-watchlist-input');
    const btn = $id('st-watchlist-add-btn');
    if (!input || !btn) return;

    const doAdd = async () => {
        const sym = input.value.trim().toUpperCase();
        if (!sym || sym.length > 10) return;
        input.value = '';
        try {
            // S18: when AutoUniverse is enabled, "+" pins the symbol
            // (sacred slot) instead of touching `cfg.ui.favorites`.
            // We also add to favorites so legacy fallbacks still see
            // it if the user later disables the AutoUniverse switch.
            if (isAutoUniverseEnabled()) {
                await pinSymbol(sym);
                try { await invoke('add_to_watchlist', { symbol: sym }); } catch { /* dup ok */ }
            } else {
                const msg = await invoke('add_to_watchlist', { symbol: sym });
                showToast(msg, 'success');
            }
            await loadWatchlist();
        } catch (err) {
            showToast(`Add failed: ${err}`, 'error');
        }
    };

    btn.addEventListener('click', doAdd);
    input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); doAdd(); }
    });
}

// ─── Live Price Update ──────────────────────────────────────────────

export function updateWatchlistPrice(symbol, price) {
    const tbody = $id('st-wl-tbody');
    if (!tbody) return;
    const row = tbody.querySelector(`tr[data-symbol="${symbol}"]`);
    if (!row) return;
    const priceEl = row.querySelector('.st-wl-price');
    if (priceEl) {
        const prev = parseFloat(priceEl.textContent.replace('$', ''));
        priceEl.textContent = `$${fmtNum(price)}`;
        if (!isNaN(prev) && prev !== price) {
            const up = price > prev;
            row.classList.remove('st-wl-flash--up', 'st-wl-flash--down');
            void row.offsetWidth;
            row.classList.add(up ? 'st-wl-flash--up' : 'st-wl-flash--down');

            const arrow = row.querySelector('.st-wl-arrow');
            if (arrow) {
                arrow.classList.remove('st-wl-arrow--up', 'st-wl-arrow--down');
                arrow.classList.add(up ? 'st-wl-arrow--up' : 'st-wl-arrow--down');
                arrow.innerHTML = up ? '\u25B2' : '\u25BC';
            }
        }
    }

    const chgEl = row.querySelector('.st-wl-chg');
    const basePrice = parseFloat(row.dataset.basePrice);
    if (chgEl && basePrice > 0) {
        const chg = ((price - basePrice) / basePrice * 100);
        const sign = chg >= 0 ? '+' : '';
        chgEl.textContent = `${sign}${fmtNum(chg)}%`;
        chgEl.className = `st-wl-chg ${chg >= 0 ? 'st-scan-chg--up' : 'st-scan-chg--down'}`;
    }
}

// ─── Sidebar Mini-Watchlist ──────────────────────────────────────────

export function renderSidebarWatchlist() {
    const container = $id('st-sidebar-watchlist');
    if (!container || disc.watchlist.length === 0) {
        if (container) container.innerHTML = '';
        return;
    }

    container.innerHTML = disc.watchlist
        .slice(0, 5)
        .map((w) => {
            const live = disc.livePrices[w.symbol];
            const priceStr = live ? `$${fmtNum(live.price)}` : '---';
            const dotCls = w.st_state === 'Active' ? 'st-sidebar-wl-dot--active'
                : w.st_state === 'Confirming' ? 'st-sidebar-wl-dot--stalking'
                : 'st-sidebar-wl-dot--idle';
            const sym = escapeHtml(w.symbol);
            return `<div class="st-sidebar-wl-row" data-symbol="${sym}">
                <span class="st-sidebar-wl-dot ${dotCls}"></span>
                <span class="st-sidebar-wl-sym">${sym}</span>
                <span class="st-sidebar-wl-price">${priceStr}</span>
            </div>`;
        })
        .join('');

    container.querySelectorAll('.st-sidebar-wl-row').forEach((row) => {
        row.addEventListener('click', () => {
            const sym = row.dataset.symbol;
            if (sym) switchSymbol(sym);
        });
    });
}

// ─── Tooltip Hover ──────────────────────────────────────────────────

export function wireTooltipHover(target) {
    const balEl = $id(`st-wallet-${target}-bal`);
    const tipEl = $id(`st-wallet-${target}-tooltip`);
    if (!balEl || !tipEl) return;
    balEl.addEventListener('mouseenter', () => tipEl.classList.remove('st-wallet-tooltip--hidden'));
    balEl.addEventListener('mouseleave', () => tipEl.classList.add('st-wallet-tooltip--hidden'));
}
