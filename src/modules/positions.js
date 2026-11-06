import { playFillBeep } from './audio.js';
import { on, emit } from './bus.js';
import { escapeHtml } from './esc.js';
import { showToast } from './toast.js';
import { toggleTradingMode } from './strategy.js';

import { invoke, listen } from './tauri-bridge.js';

const $id = (id) => document.getElementById(id);

let _holdInterval = null;
let _pollInterval = null;
let _positions = [];
let _maxHoldMinutes = 120;
let _maxPositions = 5;
let _activeSubtab = 'open';
let _allocUserBalance = 0;

// ─── Formatting ─────────────────────────────────────────────────────

function fmtPnl(val) {
    const sign = val >= 0 ? '+' : '';
    return `${sign}$${val.toFixed(2)}`;
}

function fmtPct(val) {
    const sign = val >= 0 ? '+' : '';
    return `${sign}${val.toFixed(2)}%`;
}

function fmtHold(openedAt) {
    if (!openedAt) return '---';
    const now = Math.floor(Date.now() / 1000);
    const secs = Math.max(0, now - openedAt);
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    return `${m}m ${String(s).padStart(2, '0')}s`;
}

function fmtTf(tf) {
    const t = (tf || '').trim();
    return t.length > 0 ? t : '—';
}

function isHoldWarning(openedAt) {
    if (!openedAt) return false;
    const now = Math.floor(Date.now() / 1000);
    return (now - openedAt) >= _maxHoldMinutes * 60;
}

// ─── Render ─────────────────────────────────────────────────────────

function renderPositions(positions) {
    const tbody = $id('st-positions-body');
    const emptyEl = $id('st-pos-empty');
    const table = document.querySelector('.st-positions-table');
    if (!tbody) return;

    _positions = positions;

    const posCountEl = $id('st-sidebar-pos-count');
    if (posCountEl) {
        const max = _maxPositions || 5;
        posCountEl.textContent = `${positions.length} / ${max}`;
        posCountEl.className = positions.length >= max
            ? 'hud-value hud-value--red'
            : positions.length > 0
                ? 'hud-value hud-value--green'
                : 'hud-value';
    }

    if (!positions || positions.length === 0) {
        tbody.innerHTML = '';
        if (table) table.style.display = 'none';
        if (emptyEl) emptyEl.style.display = 'block';
        return;
    }

    if (table) table.style.display = 'table';
    if (emptyEl) emptyEl.style.display = 'none';

    const rows = positions.map((p) => {
        // Backend serializes Decimal qty / avg_entry as strings via
        // serde_decimal. Coerce to Number explicitly — `String.toFixed`
        // does not exist and would throw, aborting the entire render
        // chain (and silently breaking updateMetrics() downstream).
        const qty = Number(p.qty) || 0;
        const avgEntry = Number(p.avg_entry) || 0;
        const currentPrice = Number(p.current_price) || 0;
        const pnl = Number(p.unrealized_pnl) || 0;
        const pnlPct = (avgEntry > 0 && qty > 0)
            ? (pnl / (avgEntry * Math.abs(qty))) * 100
            : 0;
        const pnlClass = pnl >= 0 ? 'st-pos-pnl--profit' : 'st-pos-pnl--loss';
        const sideClass = p.side === 'Long' ? 'st-pos-side--long' : 'st-pos-side--short';
        const holdStr = fmtHold(p.opened_at);
        const holdClass = isHoldWarning(p.opened_at) ? 'st-pos-hold--warning' : '';
        const entryTf = (p.entry_timeframe || '').trim();
        const tfLabel = fmtTf(entryTf);
        const sym = escapeHtml(p.symbol);
        const srcBadge = p.source === 'bot'
            ? '<span class="st-pos-badge st-pos-badge--bot">BOT</span>'
            : p.source === 'user'
                ? '<span class="st-pos-badge st-pos-badge--man">MAN</span>'
                : '';

        return `<tr data-symbol="${sym}" data-entry-tf="${escapeHtml(entryTf)}">
            <td class="st-pos-sym st-pos-sym--link" title="Show on chart">${sym}${srcBadge}</td>
            <td class="${sideClass}">${escapeHtml(p.side)}</td>
            <td>${Math.abs(qty).toFixed(2)}</td>
            <td>$${avgEntry.toFixed(2)}</td>
            <td>$${currentPrice.toFixed(2)}</td>
            <td class="${pnlClass}">${fmtPnl(pnl)}</td>
            <td class="${pnlClass}">${fmtPct(pnlPct)}</td>
            <td class="st-pos-hold ${holdClass}">${holdStr}</td>
            <td class="st-pos-tf${entryTf ? ' st-pos-tf--link' : ''}" title="Show ${sym} on ${tfLabel} chart" data-tf-sym="${sym}" data-tf-val="${escapeHtml(entryTf)}">📊 ${escapeHtml(tfLabel)}</td>
            <td>
                <button class="st-pos-btn-partial" data-partial-sym="${sym}" data-partial-pct="25">25%</button>
                <button class="st-pos-btn-partial" data-partial-sym="${sym}" data-partial-pct="50">50%</button>
                <button class="st-pos-btn-close" data-close-sym="${sym}">CLOSE</button>
                <button class="st-pos-btn-force" data-force-sym="${sym}" title="Force close">&#10005;</button>
            </td>
        </tr>`;
    });

    tbody.innerHTML = rows.join('');
}

// ─── Update Metrics ─────────────────────────────────────────────────

function updateMetrics(data) {
    const pnlEl = $id('st-pos-pnl-today');
    if (pnlEl) {
        pnlEl.textContent = fmtPnl(data.realized_pnl_today);
        pnlEl.className = 'hud-value hud-value--mono';
        pnlEl.classList.add(data.realized_pnl_today >= 0 ? 'hud-value--green' : 'hud-value--red');
    }

    const tradesEl = $id('st-pos-trades-today');
    if (tradesEl) tradesEl.textContent = String(data.trade_count_today);

    const ddEl = $id('st-pos-dd');
    if (ddEl) {
        ddEl.textContent = `${data.drawdown_pct.toFixed(1)}%`;
        ddEl.className = 'hud-value hud-value--mono';
        if (data.drawdown_pct > 3) ddEl.classList.add('hud-value--red');
        else if (data.drawdown_pct > 1) ddEl.classList.add('hud-value--gold');
    }

    const modeBtn = $id('st-mode-toggle');
    const modeLabel = $id('st-mode-label');
    if (modeBtn && modeLabel) {
        modeLabel.textContent = data.mode;
        modeBtn.className = 'st-mode-toggle';
        if (data.mode === 'Auto') modeBtn.classList.add('st-mode-toggle--auto');
    }

    updateWallet({
        equity: data.equity || data.capital,
        daily_pnl: data.realized_pnl_today,
        drawdown_pct: data.drawdown_pct,
        cash: data.cash || 0,
        buying_power: data.buying_power || 0,
    });

    const sidebarMode = $id('st-mode');
    if (sidebarMode) {
        sidebarMode.textContent = data.mode;
        sidebarMode.classList.toggle('st-sidebar-mode-btn--auto', data.mode === 'Auto');
    }
}

// ─── Wallet Widget ──────────────────────────────────────────────────

function fmtMoney(val) {
    if (Math.abs(val) >= 1_000_000) return `$${(val / 1_000_000).toFixed(2)}M`;
    if (Math.abs(val) >= 1_000) return `$${(val / 1_000).toFixed(1)}K`;
    return `$${val.toFixed(2)}`;
}

function updateWallet(w) {
    const eqEl = $id('st-wallet-equity');
    if (eqEl && w.equity > 0) eqEl.textContent = fmtMoney(w.equity);

    const pnlEl = $id('st-wallet-pnl');
    if (pnlEl) {
        pnlEl.textContent = fmtPnl(w.daily_pnl);
        pnlEl.className = 'st-wallet-pnl';
        pnlEl.classList.add(w.daily_pnl >= 0 ? 'st-wallet-pnl--up' : 'st-wallet-pnl--down');
    }

    const ddEl = $id('st-acct-dd');
    if (ddEl) {
        ddEl.textContent = `${w.drawdown_pct.toFixed(1)}%`;
        ddEl.className = 'hud-value hud-value--mono';
        if (w.drawdown_pct > 3) ddEl.classList.add('hud-value--red');
        else if (w.drawdown_pct > 1) ddEl.classList.add('hud-value--gold');
    }

    const ddFill = $id('st-wallet-dd-fill');
    if (ddFill) ddFill.style.width = `${Math.min(w.drawdown_pct * 10, 100)}%`;

    const cashEl = $id('st-wallet-cash');
    if (cashEl) cashEl.textContent = fmtMoney(w.cash);

    const bpEl = $id('st-wallet-bp');
    if (bpEl) bpEl.textContent = fmtMoney(w.buying_power);
}

// ─── Polling ────────────────────────────────────────────────────────

async function pollTradingStatus() {
    let data;
    try {
        data = await invoke('get_trading_status');
    } catch (err) {
        console.error('[positions] Status poll failed:', err);
        return;
    }
    if (!data) return;
    if (data.max_positions) _maxPositions = data.max_positions;
    // Render and metric updates are isolated so a transient render
    // bug (e.g. malformed position field) cannot freeze the mode
    // toggle / sidebar metrics that depend on updateMetrics().
    try { renderPositions(data.positions || []); }
    catch (err) { console.error('[positions] renderPositions failed:', err); }
    try { updateMetrics(data); }
    catch (err) { console.error('[positions] updateMetrics failed:', err); }
}

// ─── Hold Timer ─────────────────────────────────────────────────────

function refreshHoldTimers() {
    const rows = document.querySelectorAll('#st-positions-body tr');
    rows.forEach((row) => {
        const sym = row.getAttribute('data-symbol');
        const pos = _positions.find((p) => p.symbol === sym);
        if (!pos) return;
        const holdCell = row.querySelector('.st-pos-hold');
        if (holdCell) {
            holdCell.textContent = fmtHold(pos.opened_at);
            if (isHoldWarning(pos.opened_at)) {
                holdCell.classList.add('st-pos-hold--warning');
            } else {
                holdCell.classList.remove('st-pos-hold--warning');
            }
        }
    });
}

// ─── Actions ────────────────────────────────────────────────────────

async function handleClose(symbol) {
    try {
        const result = await invoke('close_position', { symbol });
        showToast(result, 'success');
        emit('position-closed', { symbol });
        pollTradingStatus();
    } catch (err) {
        showToast(`Close failed: ${normalizeError(err)}`, 'error');
    }
}

async function handleForceClose(symbol) {
    try {
        const result = await invoke('close_position', { symbol });
        showToast(`Force close: ${result}`, 'warning');
        emit('position-closed', { symbol });
        pollTradingStatus();
    } catch (err) {
        showToast(`Force close failed: ${normalizeError(err)}`, 'error');
    }
}

async function handlePartialClose(symbol, percent) {
    try {
        const result = await invoke('close_position_partial', { symbol, percent });
        showToast(result, 'success');
        emit('position-closed', { symbol });
        pollTradingStatus();
    } catch (err) {
        showToast(`Partial close failed: ${normalizeError(err)}`, 'error');
    }
}

async function syncChartToPosition(symbol, tf) {
    try {
        await invoke('set_active_symbol', { symbol });
        emit('symbol-changed', { symbol });
        if (tf) emit('tf-changed', { tf });
    } catch (err) {
        showToast(`Chart sync failed: ${normalizeError(err)}`, 'error');
    }
}

async function handleToggleMode() {
    await toggleTradingMode({ onResult: () => pollTradingStatus() });
}

export async function handleLiquidateAll() {
    if (!confirm('LIQUIDATE ALL positions? This cannot be undone.')) return;
    try {
        const result = await invoke('liquidate_all', { confirmToken: 'CONFIRM_LIQUIDATE' });
        showToast(result, 'warning');
        pollTradingStatus();
    } catch (err) {
        showToast(`Liquidate failed: ${normalizeError(err)}`, 'error');
    }
}

export async function handleEmergencyLiquidate() {
    try {
        const result = await invoke('emergency_liquidate', { confirmToken: 'CONFIRM_EMERGENCY' });
        showToast(result, 'error');
        pollTradingStatus();
    } catch (err) {
        showToast(`Emergency liquidate failed: ${normalizeError(err)}`, 'error');
    }
}

function normalizeError(err) {
    if (typeof err === 'string') return err;
    if (err && err.message) return err.message;
    return JSON.stringify(err);
}

// ─── Flash on fill ──────────────────────────────────────────────────

function flashRow(symbol, pnl) {
    const row = document.querySelector(`#st-positions-body tr[data-symbol="${symbol}"]`);
    if (!row) return;
    const cls = pnl >= 0 ? 'st-pos-flash--profit' : 'st-pos-flash--loss';
    row.classList.add(cls);
    setTimeout(() => row.classList.remove(cls), 350);
}

// ─── Order History ───────────────────────────────────────────────────

function fmtTime(epoch) {
    if (!epoch) return '---';
    const d = new Date(epoch * 1000);
    return d.toLocaleString('en-US', {
        timeZone: 'America/New_York',
        month: 'short', day: '2-digit',
        hour: '2-digit', minute: '2-digit', second: '2-digit',
        hour12: false,
    });
}

function statusBadge(status) {
    const cls = `st-order-status st-order-status--${status.replace(/\s+/g, '_')}`;
    return `<span class="${cls}">${escapeHtml(status.toUpperCase())}</span>`;
}

async function loadOrderHistory() {
    try {
        const data = await invoke('get_order_history');
        renderOrders(data);
    } catch (err) {
        console.error('[positions] Order history load failed:', err);
    }
}

function renderOrders(orders) {
    const tbody = $id('st-orders-body');
    const emptyEl = $id('st-orders-empty');
    const table = $id('st-orders-table');
    if (!tbody) return;

    const recent = orders.slice().reverse().slice(0, 100);

    if (recent.length === 0) {
        tbody.innerHTML = '';
        if (table) table.style.display = 'none';
        if (emptyEl) emptyEl.style.display = 'block';
        return;
    }

    if (table) table.style.display = 'table';
    if (emptyEl) emptyEl.style.display = 'none';

    const rows = recent.map((o) => {
        const sideClass = o.side === 'buy' ? 'st-pos-side--long' : 'st-pos-side--short';
        // Decimal fields arrive as strings; coerce before .toFixed.
        const qty = Number(o.qty) || 0;
        const fillPrice = o.filled_price != null ? Number(o.filled_price) : null;
        const fillStr = fillPrice != null && !Number.isNaN(fillPrice)
            ? `$${fillPrice.toFixed(2)}` : '---';
        return `<tr>
            <td>${fmtTime(o.submitted_at)}</td>
            <td>${escapeHtml(o.symbol)}</td>
            <td class="${sideClass}">${escapeHtml(o.side.toUpperCase())}</td>
            <td>${escapeHtml(o.order_type)}</td>
            <td>${qty.toFixed(4)}</td>
            <td>${statusBadge(o.status)}</td>
            <td>${fillStr}</td>
            <td>${escapeHtml(o.source || '---')}</td>
        </tr>`;
    });

    tbody.innerHTML = rows.join('');
}

// ─── Allocate Modal ─────────────────────────────────────────────────

export function showAllocModal(userBalance) {
    _allocUserBalance = userBalance;
    const modal = $id('st-alloc-modal');
    const balEl = $id('st-alloc-user-bal');
    if (balEl) balEl.textContent = `$${userBalance.toFixed(2)}`;
    if (modal) modal.classList.remove('st-alloc-modal--hidden');
}

function hideAllocModal() {
    const modal = $id('st-alloc-modal');
    if (modal) modal.classList.add('st-alloc-modal--hidden');
}

// ─── Event Listeners ────────────────────────────────────────────────

function setupListeners() {
    const tbody = $id('st-positions-body');
    if (tbody) {
        tbody.addEventListener('click', (e) => {
            const partialBtn = e.target.closest('.st-pos-btn-partial');
            if (partialBtn) {
                const sym = partialBtn.getAttribute('data-partial-sym');
                const pct = parseFloat(partialBtn.getAttribute('data-partial-pct'));
                if (sym && pct > 0) handlePartialClose(sym, pct);
                return;
            }
            const closeBtn = e.target.closest('.st-pos-btn-close');
            if (closeBtn) {
                const sym = closeBtn.getAttribute('data-close-sym');
                if (sym) handleClose(sym);
                return;
            }
            const forceBtn = e.target.closest('.st-pos-btn-force');
            if (forceBtn) {
                const sym = forceBtn.getAttribute('data-force-sym');
                if (sym) handleForceClose(sym);
                return;
            }
            // TF cell click → sync chart to symbol + entry TF, show arrows
            const tfCell = e.target.closest('.st-pos-tf--link');
            if (tfCell) {
                const tfSym = tfCell.getAttribute('data-tf-sym');
                const tfVal = tfCell.getAttribute('data-tf-val');
                if (tfSym && tfVal) {
                    syncChartToPosition(tfSym, tfVal);
                    showToast(`Chart → ${tfSym} ${tfVal} (entry/exit arrows)`, 'success');
                }
                return;
            }
            const row = e.target.closest('tr[data-symbol]');
            if (!row || e.target.closest('button')) return;
            const sym = row.getAttribute('data-symbol');
            if (!sym) return;
            const tf = row.getAttribute('data-entry-tf') || '';
            syncChartToPosition(sym, tf);
        });
    }

    // Sub-tab switching
    const subtabBar = document.querySelector('.st-pos-subtabs');
    if (subtabBar) {
        subtabBar.addEventListener('click', (e) => {
            const btn = e.target.closest('.st-pos-subtab');
            if (!btn) return;
            const tab = btn.getAttribute('data-subtab');
            if (!tab || tab === _activeSubtab) return;
            _activeSubtab = tab;
            subtabBar.querySelectorAll('.st-pos-subtab').forEach((b) => b.classList.remove('st-pos-subtab--active'));
            btn.classList.add('st-pos-subtab--active');
            const openPanel = $id('st-pos-panel-open');
            const ordersPanel = $id('st-pos-panel-orders');
            if (tab === 'open') {
                if (openPanel) openPanel.classList.remove('st-pos-panel--hidden');
                if (ordersPanel) ordersPanel.classList.add('st-pos-panel--hidden');
            } else {
                if (openPanel) openPanel.classList.add('st-pos-panel--hidden');
                if (ordersPanel) ordersPanel.classList.remove('st-pos-panel--hidden');
                loadOrderHistory();
            }
        });
    }

    // Allocate modal buttons
    const allocBtns = document.querySelector('.st-alloc-buttons');
    if (allocBtns) {
        allocBtns.addEventListener('click', async (e) => {
            const btn = e.target.closest('.st-alloc-btn');
            if (!btn) return;
            const pct = parseInt(btn.getAttribute('data-alloc-pct'), 10);
            if (!pct || _allocUserBalance <= 0) return;
            const amount = _allocUserBalance * pct / 100;
            try {
                await invoke('topup_bot', { amount });
                showToast(`Allocated $${amount.toFixed(2)} to bot wallet`, 'success');
                emit('wallet-changed', {});
            } catch (err) {
                showToast(`Allocation failed: ${normalizeError(err)}`, 'error');
            }
            hideAllocModal();
        });
    }
    const skipBtn = $id('st-alloc-skip');
    if (skipBtn) skipBtn.addEventListener('click', hideAllocModal);

    document.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') {
            const modal = $id('st-alloc-modal');
            if (modal && !modal.classList.contains('st-alloc-modal--hidden')) {
                hideAllocModal();
                e.stopPropagation();
            }
        }
    });

    const modeBtn = $id('st-mode-toggle');
    if (modeBtn) modeBtn.addEventListener('click', handleToggleMode);

    const liqBtn = $id('st-liquidate-btn');
    if (liqBtn) liqBtn.addEventListener('click', handleLiquidateAll);

    listen('equity-sync', (event) => {
        const p = event.payload;
        if (p) updateWallet(p);
    });

    listen('positions-sync', () => pollTradingStatus());
    listen('trade-fill', async (event) => {
        const p = event.payload;
        if (p && p.symbol) flashRow(p.symbol, p.pnl || 0);
        pollTradingStatus();
        if (_activeSubtab === 'orders') loadOrderHistory();
        try {
            const cfg = await invoke('get_config');
            if (cfg?.ui?.sound_on_fill !== false) playFillBeep();
        } catch (_) { /* config unavailable, skip sound */ }
    });
    listen('order-lifecycle', () => {
        if (_activeSubtab === 'orders') loadOrderHistory();
    });
    listen('bot-pnl', () => pollTradingStatus());
    listen('mode-changed', () => pollTradingStatus());
    listen('positions-liquidated', () => pollTradingStatus());

    on('position-closed', () => pollTradingStatus());
}

// ─── Max hold config ────────────────────────────────────────────────

async function loadMaxHold() {
    try {
        const cfg = await invoke('get_config');
        if (cfg && cfg.smarttrend && cfg.smarttrend.max_hold_minutes) {
            _maxHoldMinutes = cfg.smarttrend.max_hold_minutes;
        }
    } catch { /* use default */ }
}

// ─── Init ───────────────────────────────────────────────────────────

export async function initPositions() {
    await loadMaxHold();
    setupListeners();
    await pollTradingStatus();
    _pollInterval = setInterval(pollTradingStatus, 3000);
    _holdInterval = setInterval(refreshHoldTimers, 1000);
}

export function disposePositions() {
    if (_pollInterval) { clearInterval(_pollInterval); _pollInterval = null; }
    if (_holdInterval) { clearInterval(_holdInterval); _holdInterval = null; }
}
