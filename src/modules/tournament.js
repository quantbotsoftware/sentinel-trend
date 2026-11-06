// D Evolution Protocol (Sesiunea 1) — Tournament leaderboard.
//
// Renders the per-strategy P/L attribution panel that lives in the
// "TOURNAMENT" tab. Subscribes to the backend `tournament-update`
// event for push-based refresh; re-fetches on tab activation as a
// safety net (in case an event was dropped while the tab was hidden).

import { invoke, listen } from './tauri-bridge.js';
import { showToast } from './toast.js';
import { fmtNum } from './fmt.js';
import { strategyLabel } from './strategy-labels.js';

const TAB_BTN_SELECTOR = '.st-tab-btn[data-tab="tournament"]';
const TAB_CONTENT_ID = 'st-tab-tournament';
const REFRESH_BTN_ID = 'st-tour-refresh-btn';
const BODY_ID = 'st-tour-body';
const EMPTY_ID = 'st-tour-empty';
const CAPITAL_ID = 'st-tour-capital';
const POOL_ID = 'st-tour-pool';
const SAVED_ID = 'st-tour-saved';
const REGIME_ID = 'st-tour-regime';

let unsubscribe = null;
let unsubscribeRegime = null;
let lastRendered = null;
let lastRegime = null;

function fmtCurrency(n) {
  if (typeof n !== 'number' || !isFinite(n)) return '$—';
  const sign = n < 0 ? '-' : '';
  const abs = Math.abs(n);
  return `${sign}$${abs.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })}`;
}

function fmtPct(n) {
  if (typeof n !== 'number' || !isFinite(n)) return '—';
  return `${n.toFixed(1)}%`;
}

function fmtAge(ts) {
  if (!ts || ts <= 0) return '—';
  const date = new Date(ts * 1000);
  return date.toLocaleString();
}

function statusBadge(status, pendingSandbox) {
  if (pendingSandbox) {
    return `<span class="st-tour-badge st-tour-badge--sandbox" title="Strategy is degraded; A/B sandbox simulation validation pending at 20:00 ET. Capital allocation cut by 50%.">SANDBOX</span>`;
  }
  const cls =
    status === 'active'
      ? 'st-tour-badge--active'
      : status === 'draining'
        ? 'st-tour-badge--draining'
        : 'st-tour-badge--dead';
  return `<span class="st-tour-badge ${cls}">${status}</span>`;
}

function streakBadge(len) {
  if (!len || len === 0) return '—';
  const cls = len > 0 ? 'st-tour-streak-win' : 'st-tour-streak-loss';
  const sign = len > 0 ? '+' : '';
  return `<span class="${cls}">${sign}${len}</span>`;
}

function actionButtons(strategy, status) {
  const buttons = [];
  if (status === 'active') {
    buttons.push(
      `<button class="st-tour-btn st-tour-btn--drain" data-action="drain" data-strategy="${strategy}" title="Drain — refuse new entries, let open positions exit naturally">DRAIN</button>`
    );
  }
  if (status === 'draining' || status === 'active') {
    buttons.push(
      `<button class="st-tour-btn st-tour-btn--kill" data-action="kill" data-strategy="${strategy}" title="Kill — return sub-wallet to pool, no further entries">KILL</button>`
    );
  }
  if (status === 'dead' || status === 'draining') {
    buttons.push(
      `<button class="st-tour-btn st-tour-btn--resurrect" data-action="resurrect" data-strategy="${strategy}" title="Resurrect — re-allocate sub-wallet from pool, set Active">RESURRECT</button>`
    );
  }
  return buttons.join('');
}

function rowFor(row) {
  const pnlCls = row.cumulative_pnl > 0 ? 'st-pl-win' : row.cumulative_pnl < 0 ? 'st-pl-loss' : '';
  const expCls = row.expectancy > 0 ? 'st-pl-win' : row.expectancy < 0 ? 'st-pl-loss' : '';
  return `<tr data-strategy="${row.strategy}">
    <td class="st-tour-strategy">${strategyLabel(row.strategy)}</td>
    <td>${statusBadge(row.status, row.pending_sandbox)}</td>
    <td class="num">${row.trades}</td>
    <td class="num">${fmtPct(row.win_rate_pct)}</td>
    <td class="num ${expCls}">${fmtCurrency(row.expectancy)}</td>
    <td class="num ${pnlCls}">${fmtCurrency(row.cumulative_pnl)}</td>
    <td class="num">${fmtPct(row.max_drawdown_pct)}</td>
    <td class="num">${fmtCurrency(row.current_sub_wallet)}</td>
    <td class="num">${streakBadge(row.current_streak_len)}</td>
    <td>${actionButtons(row.strategy, row.status)}</td>
  </tr>`;
}

function render(state) {
  lastRendered = state;
  const capEl = document.getElementById(CAPITAL_ID);
  const poolEl = document.getElementById(POOL_ID);
  const savedEl = document.getElementById(SAVED_ID);
  const bodyEl = document.getElementById(BODY_ID);
  const emptyEl = document.getElementById(EMPTY_ID);
  if (!capEl || !poolEl || !savedEl || !bodyEl || !emptyEl) return;

  capEl.textContent = fmtCurrency(state.paper_capital);
  poolEl.textContent = fmtCurrency(state.free_pool);
  savedEl.textContent = fmtAge(state.last_saved_at);

  const rows = state.leaderboard || [];
  if (rows.length === 0) {
    bodyEl.innerHTML = '';
    emptyEl.style.display = 'block';
    return;
  }
  emptyEl.style.display = 'none';
  bodyEl.innerHTML = rows.map(rowFor).join('');
}

async function fetchAndRender() {
  try {
    const state = await invoke('get_tournament_state');
    render(state);
  } catch (e) {
    console.warn('tournament fetch failed', e);
    showToast(`Tournament fetch failed: ${e}`, { kind: 'warn' });
  }
}

function bindActionDelegation() {
  const bodyEl = document.getElementById(BODY_ID);
  if (!bodyEl) return;
  bodyEl.addEventListener('click', async (ev) => {
    const btn = ev.target.closest('button.st-tour-btn');
    if (!btn) return;
    const action = btn.dataset.action;
    const strategy = btn.dataset.strategy;
    if (!action || !strategy) return;
    const verb = action.toUpperCase();
    const label = strategyLabel(strategy);
    if (!confirm(`${verb} strategy ${label}?`)) return;
    btn.disabled = true;
    try {
      const cmd = `${action}_strategy`;
      const ok = await invoke(cmd, { payload: { strategy } });
      showToast(`${verb} ${label}: ${ok ? 'success' : 'no-op'}`, {
        kind: ok ? 'info' : 'warn',
      });
      await fetchAndRender();
    } catch (e) {
      showToast(`${verb} ${label} failed: ${e}`, { kind: 'error' });
    } finally {
      btn.disabled = false;
    }
  });
}

function bindRefreshButton() {
  const btn = document.getElementById(REFRESH_BTN_ID);
  if (!btn) return;
  btn.addEventListener('click', () => {
    fetchAndRender();
  });
}

function bindTabActivation() {
  const btn = document.querySelector(TAB_BTN_SELECTOR);
  if (!btn) return;
  btn.addEventListener('click', () => {
    // Tab system handled elsewhere; we just piggy-back to refresh
    setTimeout(fetchAndRender, 50);
  });
}

async function bindLiveEvent() {
  try {
    if (unsubscribe) {
      unsubscribe();
      unsubscribe = null;
    }
    unsubscribe = await listen('tournament-update', (_payload) => {
      // Don't fetch unconditionally; only if the tab is visible OR
      // we have nothing rendered yet. The tab activation handler
      // covers the hidden→visible case.
      const tabEl = document.getElementById(TAB_CONTENT_ID);
      if (!tabEl) return;
      const isVisible = !tabEl.classList.contains('st-tab-content--hidden');
      if (isVisible || lastRendered === null) {
        fetchAndRender();
      }
    });
  } catch (e) {
    console.warn('tournament-update listener failed', e);
  }
}

function renderRegime(regime, payload) {
  const el = document.getElementById(REGIME_ID);
  if (!el) return;
  const r = (regime || 'unknown').toLowerCase();
  el.dataset.regime = r;
  el.textContent = r.toUpperCase();
  if (payload && typeof payload.realised_vol_annualised_pct === 'number') {
    const vol = payload.realised_vol_annualised_pct.toFixed(1);
    const slope = (payload.trend_slope_pct ?? 0).toFixed(2);
    el.title = `Regime: ${r.toUpperCase()} — vol ${vol}% / slope ${slope}% / r² ${(payload.trend_r2 ?? 0).toFixed(2)}`;
  } else {
    el.title = `Regime: ${r.toUpperCase()}`;
  }
}

async function bindRegimeEvent() {
  try {
    if (unsubscribeRegime) {
      unsubscribeRegime();
      unsubscribeRegime = null;
    }
    unsubscribeRegime = await listen('regime-change', (event) => {
      const payload = event && event.payload ? event.payload : {};
      const regime = payload.regime || 'unknown';
      // Avoid spamming toasts on initial publish (None → first regime)
      const isFirst = lastRegime === null;
      lastRegime = regime;
      renderRegime(regime, payload);
      if (!isFirst) {
        showToast(`Market regime → ${regime.toUpperCase()}`, { kind: 'info' });
      }
      // Force a tournament refresh so the leaderboard reflects the
      // new regime context — even if the tab is currently hidden,
      // the next activation will pick up the cached state.
      fetchAndRender();
    });
  } catch (e) {
    console.warn('regime-change listener failed', e);
  }
}

export function initTournament() {
  bindActionDelegation();
  bindRefreshButton();
  bindTabActivation();
  bindLiveEvent();
  bindRegimeEvent();
  // Initial fetch — populate leaderboard state on bot boot
  setTimeout(fetchAndRender, 250);
}

export function disposeTournament() {
  if (unsubscribe) {
    try {
      unsubscribe();
    } catch (_) {}
    unsubscribe = null;
  }
  if (unsubscribeRegime) {
    try {
      unsubscribeRegime();
    } catch (_) {}
    unsubscribeRegime = null;
  }
  lastRendered = null;
  lastRegime = null;
}

// expose for debug
if (typeof window !== 'undefined') {
  window.__sentinel_tournament = { fetchAndRender, render };
  // for fmtNum users elsewhere
  void fmtNum;
}
