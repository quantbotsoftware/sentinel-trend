// S18 — AutoUniverse frontend integration
//
// Wires the bottom Self-Curating Watchlist panel to the backend
// commands and live events. Surfaces pinned / auto / blacklist
// state as overlays on the existing watchlist rows (badges) and
// in a collapsible blacklist sub-panel.
//
// All UI mutations route through Tauri commands so the canonical
// state always lives in `state.market.auto_universe` (Rust side).
// The frontend only caches the latest snapshot for rendering.

import { escapeHtml } from '../esc.js';
import { showToast } from '../toast.js';
import { invoke, listen } from '../tauri-bridge.js';
import { $id, disc } from './index.js';

// ─── Local cache of the latest snapshot ─────────────────────────────

const au = {
    enabled: false,
    pinned: [],            // string[]
    auto_pool: [],         // AutoEntry[]
    blacklist: [],         // BlacklistEntry[]
    max_total_slots: 20,
    max_pinned_slots: 8,
};

// Exposed so other modules (e.g. watchlist row renderer) can ask
// "what badge should I show for this symbol?".
export function symbolBadge(symbol) {
    if (!au.enabled) return null;
    const key = String(symbol || '').toUpperCase();
    if (au.pinned.some((s) => s.toUpperCase() === key)) return 'PIN';
    if (au.auto_pool.some((e) => e.symbol.toUpperCase() === key)) return 'AUTO';
    if (au.blacklist.some((e) => e.symbol.toUpperCase() === key)) return 'BAN';
    return null;
}

export function isAutoUniverseEnabled() {
    return au.enabled;
}

export function getAutoUniverseSnapshot() {
    return au;
}

// ─── Status bar render ──────────────────────────────────────────────

function renderStatusBar() {
    const cb = $id('st-au-enabled-cb');
    if (cb) cb.checked = !!au.enabled;

    const pinnedCount = $id('st-au-pinned-count');
    const pinnedCap = $id('st-au-pinned-cap');
    if (pinnedCount) pinnedCount.textContent = String(au.pinned.length);
    if (pinnedCap) pinnedCap.textContent = `/${au.max_pinned_slots}`;

    const autoCount = $id('st-au-auto-count');
    if (autoCount) autoCount.textContent = String(au.auto_pool.length);

    const blCount = $id('st-au-bl-count');
    if (blCount) blCount.textContent = String(au.blacklist.length);

    const status = $id('st-au-status');
    if (status) {
        if (au.enabled) {
            const slots = au.pinned.length + au.auto_pool.length;
            status.textContent = `active · ${slots}/${au.max_total_slots} slots`;
            status.className = 'st-au-status st-au-status--on';
        } else {
            status.textContent = 'disabled (legacy: trade-from-favorites)';
            status.className = 'st-au-status st-au-status--off';
        }
    }
}

// ─── Blacklist panel render ─────────────────────────────────────────

function fmtRemainingSecs(banned_at, ttl_hours) {
    if (!ttl_hours || ttl_hours === 0) return 'until reset';
    const banned = new Date(banned_at).getTime();
    const expires = banned + ttl_hours * 3600 * 1000;
    const remMs = expires - Date.now();
    if (remMs <= 0) return 'expiring…';
    const remSec = Math.floor(remMs / 1000);
    const h = Math.floor(remSec / 3600);
    const m = Math.floor((remSec % 3600) / 60);
    if (h >= 1) return `${h}h ${m}m`;
    const s = remSec % 60;
    return `${m}m ${s}s`;
}

function reasonLabel(reason) {
    switch (reason) {
        case 'ConsecutiveLosses': return '2+ losses';
        case 'BigLoss': return 'big loss';
        case 'ScoreDecay': return 'score decay';
        case 'Stale': return 'stale catalyst';
        case 'Manual': return 'manual';
        default: return String(reason || 'unknown');
    }
}

function renderBlacklist() {
    const list = $id('st-au-bl-list');
    if (!list) return;
    if (au.blacklist.length === 0) {
        list.innerHTML = '<div class="st-au-bl-empty">No symbols blacklisted</div>';
        return;
    }
    list.innerHTML = au.blacklist
        .map((b) => {
            const sym = escapeHtml(b.symbol);
            const rem = fmtRemainingSecs(b.banned_at, b.ttl_hours);
            const reason = escapeHtml(reasonLabel(b.reason));
            const pnl = (b.last_loss_amount || 0).toFixed(2);
            return `<div class="st-au-bl-row" data-symbol="${sym}">
                <span class="st-au-bl-sym">${sym}</span>
                <span class="st-au-bl-reason">${reason}</span>
                <span class="st-au-bl-pnl">$${pnl}</span>
                <span class="st-au-bl-ttl">${escapeHtml(rem)}</span>
                <button class="st-au-bl-clear-one" data-symbol="${sym}" title="Clear this entry">&times;</button>
            </div>`;
        })
        .join('');
    list.querySelectorAll('.st-au-bl-clear-one').forEach((btn) => {
        btn.addEventListener('click', async (e) => {
            e.stopPropagation();
            const sym = btn.dataset.symbol;
            try {
                await invoke('clear_blacklist_entry', { symbol: sym });
                showToast(`${sym} cleared from blacklist`, 'success');
            } catch (err) {
                showToast(`Clear failed: ${err}`, 'error');
            }
        });
    });
}

function renderAll() {
    renderStatusBar();
    renderBlacklist();
}

// ─── Event handlers ─────────────────────────────────────────────────

function wireToggles() {
    const cb = $id('st-au-enabled-cb');
    if (cb) {
        cb.addEventListener('change', async () => {
            try {
                await invoke('set_auto_universe_enabled', { enabled: cb.checked });
                showToast(
                    cb.checked
                        ? 'AutoUniverse ENABLED'
                        : 'AutoUniverse disabled (legacy mode)',
                    cb.checked ? 'success' : 'info'
                );
                // Trigger a watchlist reload so badges + universe align.
                try { await invoke('get_auto_universe_state'); } catch { /* ignored */ }
            } catch (err) {
                showToast(`Toggle failed: ${err}`, 'error');
                cb.checked = !cb.checked; // revert
            }
        });
    }

    const blBtn = $id('st-au-blacklist-btn');
    const blPanel = $id('st-au-blacklist');
    if (blBtn && blPanel) {
        blBtn.addEventListener('click', () => {
            blPanel.classList.toggle('st-au-blacklist--hidden');
        });
    }

    const clearAll = $id('st-au-bl-clear-all');
    if (clearAll) {
        clearAll.addEventListener('click', async () => {
            try {
                const msg = await invoke('clear_all_blacklist');
                showToast(msg, 'success');
            } catch (err) {
                showToast(`Clear-all failed: ${err}`, 'error');
            }
        });
    }
}

function refreshTtlCountdownsPeriodically() {
    // Re-render the blacklist every 10s so the TTL countdowns advance.
    setInterval(() => {
        if (au.blacklist.length > 0) renderBlacklist();
    }, 10000);
}

// ─── Public action helpers (used by watchlist row renderer) ─────────

export async function pinSymbol(symbol) {
    try {
        const msg = await invoke('pin_symbol', { symbol });
        showToast(msg, 'success');
    } catch (err) {
        showToast(`Pin failed: ${err}`, 'error');
    }
}

export async function unpinSymbol(symbol) {
    try {
        const msg = await invoke('unpin_symbol', { symbol });
        showToast(msg, 'success');
    } catch (err) {
        showToast(`Unpin failed: ${err}`, 'error');
    }
}

export async function promoteAutoToPinned(symbol) {
    try {
        const msg = await invoke('promote_auto_to_pinned', { symbol });
        showToast(msg, 'success');
    } catch (err) {
        showToast(`Promote failed: ${err}`, 'error');
    }
}

export async function forceRemoveFromUniverse(symbol) {
    try {
        const msg = await invoke('force_remove_universe_symbol', { symbol });
        showToast(msg, 'success');
    } catch (err) {
        showToast(`Remove failed: ${err}`, 'error');
    }
}

export async function manualBlacklistSymbol(symbol, ttlHours) {
    try {
        const msg = await invoke('manual_blacklist_symbol', {
            symbol,
            ttlHours: ttlHours ?? null,
        });
        showToast(msg, 'success');
    } catch (err) {
        showToast(`Blacklist failed: ${err}`, 'error');
    }
}

// ─── Init ───────────────────────────────────────────────────────────

export async function initAutoUniverse() {
    wireToggles();
    refreshTtlCountdownsPeriodically();

    // Subscribe to live mutations from the backend.
    listen('auto-universe-state', (event) => {
        const p = event?.payload;
        if (!p) return;
        Object.assign(au, p);
        renderAll();
        // Trigger watchlist re-render so badges refresh too.
        try {
            const ev = new CustomEvent('auto-universe-changed');
            document.dispatchEvent(ev);
        } catch { /* ignored */ }
    });

    listen('auto-universe-update', (event) => {
        const r = event?.payload;
        if (!r) return;
        if (r.promoted && r.promoted.length > 0) {
            showToast(`AUTO promoted: ${r.promoted.join(', ')}`, 'info');
        }
        if (r.demoted && r.demoted.length > 0) {
            const labels = r.demoted.map(([s, why]) => `${s} (${why})`).join(', ');
            showToast(`AUTO demoted: ${labels}`, 'info');
        }
    });

    listen('auto-universe-blacklisted', (event) => {
        const r = event?.payload;
        if (!r) return;
        showToast(
            `BLACKLISTED: ${r.symbol} — ${reasonLabel(r.blacklist_reason)}`,
            'warning'
        );
    });

    // Initial fetch.
    try {
        const snap = await invoke('get_auto_universe_state');
        if (snap) {
            Object.assign(au, snap);
            renderAll();
        }
    } catch (err) {
        console.error('[auto-universe] initial fetch failed:', err);
    }
}
