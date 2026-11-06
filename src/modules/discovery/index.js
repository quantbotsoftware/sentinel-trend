import { emit, on } from '../bus.js';
import { showToast } from '../toast.js';
import { initManualOrder } from '../manual-order.js';
import { invoke, listen } from '../tauri-bridge.js';

import { renderMovers } from './movers.js';
import {
    renderWatchlist, loadWatchlist, setupLiveListeners,
    setupManualAdd, renderSidebarWatchlist,
} from './watchlist.js';
import { initWalletBar, updateWalletBalances } from './wallet.js';
import { renderScanResults, setupScanTableSort } from './scan-results.js';
import { initAutoUniverse } from './auto-universe.js';
import { initNewsModal } from './news-modal.js';
import { filterTodayRows, todayEtDateStr } from './session-today.js';

export const $id = (id) => document.getElementById(id);

const _ss = (() => { try { return JSON.parse(localStorage.getItem('st-scan-sort')); } catch { return null; } })();

export const disc = {
    movers: [],
    watchlist: [],
    livePrices: {},
    scanResults: [],
    walletPollTimer: null,
    watchlistPollTimer: null,
    activeWalletOp: null,
    scanSortCol: _ss?.col || 'score',
    scanSortAsc: _ss?.asc ?? false,
};

// ─── Sub-tab Wiring (WATCHLIST / TOP MOVERS) ────────────────────────

function wireDiscTabs() {
    const tabs = document.querySelectorAll('.st-disc-tab');
    const panels = {
        watchlist: $id('st-disc-panel-watchlist'),
        movers: $id('st-disc-panel-movers'),
    };
    const wlInput = $id('st-watchlist-input');
    const wlBtn = $id('st-watchlist-add-btn');
    if (!tabs.length) return;

    tabs.forEach((tab) => {
        tab.addEventListener('click', () => {
            const target = tab.dataset.panel;
            tabs.forEach((t) => t.classList.toggle('st-disc-tab--active', t === tab));
            Object.entries(panels).forEach(([key, el]) => {
                if (el) el.classList.toggle('st-disc-panel--hidden', key !== target);
            });
            const showWl = target === 'watchlist';
            if (wlInput) wlInput.style.display = showWl ? '' : 'none';
            if (wlBtn) wlBtn.style.display = showWl ? '' : 'none';
        });
    });
}

// ─── Symbol Switch ──────────────────────────────────────────────────

export async function switchSymbol(sym) {
    try {
        await invoke('set_active_symbol', { symbol: sym });
        emit('symbol-changed', { symbol: sym });
        showToast(`Symbol → ${sym}`, 'success');
    } catch (err) {
        showToast(`Switch failed: ${err}`, 'error');
    }
}

// ─── Bus + Scan Listeners ───────────────────────────────────────────

function setupBusListeners() {
    on('watchlist-changed', () => loadWatchlist());
    on('wallet-changed', () => updateWalletBalances());
    listen('wallet-changed', () => updateWalletBalances());
    listen('scan-results', (event) => {
        renderScanResults(event.payload || []);
        loadWatchlist();
    });

    const clearBtn = $id('st-scan-btn-clear-results');
    if (clearBtn) {
        clearBtn.addEventListener('click', () => {
            disc.scanResults = [];
            renderScanResults([]);
        });
    }
}

// ─── Public Init ────────────────────────────────────────────────────

export async function initDiscovery() {
    initWalletBar();
    initManualOrder();
    setupManualAdd();
    wireDiscTabs();
    setupLiveListeners();
    setupBusListeners();
    setupScanTableSort();
    initNewsModal();
    // S18 — AutoUniverse panel + live state listener. Must be
    // initialised BEFORE the first watchlist render so badges show
    // up correctly on the initial paint.
    await initAutoUniverse();
    document.addEventListener('auto-universe-changed', () => {
        // Re-render watchlist when auto-universe state mutates so
        // badges + per-row buttons stay aligned with the truth.
        try { renderWatchlist(); } catch { /* ignored */ }
    });

    updateSessionBadge();

    try {
        const movers = await invoke('get_movers');
        disc.movers = filterTodayRows(movers || []);
        renderMovers();
    } catch (err) {
        console.error('[discovery] Initial movers load failed:', err);
    }

    try {
        const cached = await invoke('get_scan_results');
        renderScanResults(filterTodayRows(cached || []));
    } catch { /* no cached results yet */ }

    await loadWatchlist();

    disc.watchlistPollTimer = setInterval(() => loadWatchlist(), 60000);
}

export function disposeDiscovery() {
    if (disc.walletPollTimer) { clearInterval(disc.walletPollTimer); disc.walletPollTimer = null; }
    if (disc.watchlistPollTimer) { clearInterval(disc.watchlistPollTimer); disc.watchlistPollTimer = null; }
}

export function updateSessionBadge() {
    const el = $id('st-scan-session-badge');
    if (!el) return;
    const today = todayEtDateStr();
    el.textContent = `SESSION: ${today}`;
    el.title = `Actionable scan/movers must match ET date ${today}`;
    el.classList.add('st-scan-session-badge--today');
}
