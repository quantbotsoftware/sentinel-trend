// D-PLUS (Sesiunea 2) — Frontend module.
//
// Wires the four sub-panels (Adaptive Risk, Memory DB, Causal Lead-Lag,
// Strategy Evolver) inside the new "D-PLUS" tab. Listens on push events
// from the backend (`dplus-trade-close`, `causal-graph-update`,
// `evolver-report`) for low-latency refreshes, and re-fetches on tab
// activation as a safety net.
//
// Each command returns a `{ rows, ... }` shape; render functions pull
// the relevant fields and produce minimal DOM updates.

import { invoke, listen } from './tauri-bridge.js';
import { showToast } from './toast.js';

const TAB_BTN_SELECTOR = '.st-tab-btn[data-tab="dplus"]';

let unsubTradeClose = null;
let unsubCausalUpdate = null;
let unsubEvolverReport = null;
let lastFetchTs = 0;

function el(id) {
  return document.getElementById(id);
}

function fmtAge(ts) {
  if (!ts || ts <= 0) return '—';
  const date = new Date(ts * 1000);
  return date.toLocaleString();
}

function fmtSizeMult(m) {
  if (typeof m !== 'number' || !isFinite(m)) return '—';
  return `${m.toFixed(2)}×`;
}

function sizeMultClass(m) {
  if (m < 0.3) return 'dplus-size-mult--deep';
  if (m < 0.7) return 'dplus-size-mult--throttled';
  return '';
}

function corrClass(c) {
  return c >= 0 ? 'dplus-corr-positive' : 'dplus-corr-negative';
}

// ════════════════════════════════════════════════════════════════════
//  Adaptive Risk
// ════════════════════════════════════════════════════════════════════

async function fetchAndRenderRisk() {
  const body = el('dplus-risk-body');
  const empty = el('dplus-risk-empty');
  if (!body || !empty) return;
  try {
    const out = await invoke('get_adaptive_risk_state');
    body.innerHTML = '';
    if (!out || !Array.isArray(out.rows) || out.rows.length === 0) {
      empty.style.display = '';
      return;
    }
    empty.style.display = 'none';
    const now = Math.floor(Date.now() / 1000);
    for (const r of out.rows) {
      const inCooldown = r.cooldown_until > now;
      const cooldownLabel = inCooldown
        ? `${Math.max(0, r.cooldown_until - now)}s`
        : '—';
      const tr = document.createElement('tr');
      tr.innerHTML = `
        <td>${escape(r.strategy)}</td>
        <td class="num ${sizeMultClass(r.current_size_mult)}">${fmtSizeMult(r.current_size_mult)}</td>
        <td class="num">${r.consecutive_losses}</td>
        <td class="num">${r.consecutive_wins}</td>
        <td class="num">${cooldownLabel}</td>
        <td><button class="dplus-action-btn" data-reset-strategy="${escape(r.strategy)}">RESET</button></td>
      `;
      body.appendChild(tr);
    }
  } catch (e) {
    body.innerHTML = '';
    empty.textContent = `Adaptive Risk: ${e}`;
    empty.style.display = '';
  }
}

async function resetRisk(strategy) {
  try {
    await invoke('reset_adaptive_risk', { payload: { strategy } });
    showToast(`Risk reset: ${strategy}`, 'success');
    await fetchAndRenderRisk();
  } catch (e) {
    showToast(`Reset failed: ${e}`, 'error');
  }
}

// ════════════════════════════════════════════════════════════════════
//  Memory DB
// ════════════════════════════════════════════════════════════════════

async function fetchAndRenderMemory() {
  try {
    const stats = await invoke('get_memory_db_stats');
    if (!stats) return;
    const map = {
      'dplus-mem-total': stats.total_setups,
      'dplus-mem-closed': stats.closed_setups,
      'dplus-mem-posteriors': stats.posteriors_trained,
      'dplus-mem-schema': `v${stats.schema_version}`,
    };
    for (const [id, val] of Object.entries(map)) {
      const node = el(id);
      if (node) node.textContent = String(val ?? '—');
    }
    const last = el('dplus-mem-last');
    if (last) last.textContent = fmtAge(stats.last_write_ts);
  } catch (e) {
    const last = el('dplus-mem-last');
    if (last) last.textContent = `err: ${e}`;
  }
}

// ════════════════════════════════════════════════════════════════════
//  Causal Lead-Lag
// ════════════════════════════════════════════════════════════════════

async function fetchAndRenderCausal() {
  const body = el('dplus-causal-body');
  const empty = el('dplus-causal-empty');
  if (!body || !empty) return;
  try {
    const snap = await invoke('get_lead_lag_graph');
    body.innerHTML = '';
    if (!snap || !Array.isArray(snap.edges) || snap.edges.length === 0) {
      empty.style.display = '';
      return;
    }
    empty.style.display = 'none';
    for (const e of snap.edges.slice(0, 15)) {
      const tr = document.createElement('tr');
      tr.innerHTML = `
        <td>${escape(e.leader)}</td>
        <td>${escape(e.lagger)}</td>
        <td class="num">${e.lag_secs}</td>
        <td class="num ${corrClass(e.corr)}">${e.corr.toFixed(3)}</td>
      `;
      body.appendChild(tr);
    }
  } catch (e) {
    body.innerHTML = '';
    empty.textContent = `Causal: ${e}`;
    empty.style.display = '';
  }
}

async function forceRecomputeCausal() {
  try {
    await invoke('force_recompute_causal_graph');
    await fetchAndRenderCausal();
    showToast('Lead-lag graph recomputed', 'success');
  } catch (e) {
    showToast(`Recompute failed: ${e}`, 'error');
  }
}

// ════════════════════════════════════════════════════════════════════
//  Strategy Evolver
// ════════════════════════════════════════════════════════════════════

async function fetchAndRenderEvolver() {
  const body = el('dplus-evolver-body');
  const empty = el('dplus-evolver-empty');
  const overridesNode = el('dplus-evolver-overrides');
  if (!body || !empty) return;
  try {
    const overrides = await invoke('get_param_overrides');
    if (overridesNode) {
      const lines = [];
      for (const [strategy, params] of Object.entries(
        overrides.strategies || {}
      )) {
        const parts = Object.entries(params)
          .map(([k, v]) => `${k}=${Number(v).toFixed(3)}`)
          .join(' ');
        lines.push(`${strategy} → ${parts}`);
      }
      overridesNode.textContent =
        lines.length > 0 ? lines.join('\n') : 'no overrides — using compile-time defaults';
    }
    const proposals = await invoke('list_evolver_proposals');
    body.innerHTML = '';
    if (!Array.isArray(proposals) || proposals.length === 0) {
      empty.style.display = '';
      return;
    }
    empty.style.display = 'none';
    for (const p of proposals) {
      const tr = document.createElement('tr');
      const actions =
        p.status === 'pending'
          ? `<button class="dplus-action-btn dplus-action-btn--approve" data-evolver-action="approve" data-id="${escape(p.id)}">APPROVE</button>
             <button class="dplus-action-btn dplus-action-btn--reject" data-evolver-action="reject" data-id="${escape(p.id)}">REJECT</button>`
          : `<span>${escape(p.status)}</span>`;
      tr.innerHTML = `
        <td>${escape(p.id)}</td>
        <td>${escape(p.parent_a)} + ${escape(p.parent_b)}</td>
        <td>${escape(p.status)}</td>
        <td>${actions}</td>
      `;
      body.appendChild(tr);
    }
  } catch (e) {
    body.innerHTML = '';
    empty.textContent = `Evolver: ${e}`;
    empty.style.display = '';
  }
}

async function approveProposal(id) {
  try {
    await invoke('approve_evolver_proposal', { payload: { id } });
    showToast('Proposal approved', 'success');
    await fetchAndRenderEvolver();
  } catch (e) {
    showToast(`Approve failed: ${e}`, 'error');
  }
}

async function rejectProposal(id) {
  try {
    await invoke('reject_evolver_proposal', { payload: { id } });
    showToast('Proposal rejected', 'info');
    await fetchAndRenderEvolver();
  } catch (e) {
    showToast(`Reject failed: ${e}`, 'error');
  }
}

// ════════════════════════════════════════════════════════════════════
//  Tab activation + delegated event handlers
// ════════════════════════════════════════════════════════════════════

function escape(str) {
  if (typeof str !== 'string') return '';
  return str.replace(/[&<>"']/g, (c) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  })[c]);
}

async function fetchAll(force = false) {
  const now = Date.now();
  if (!force && now - lastFetchTs < 500) return; // debounce burst events
  lastFetchTs = now;
  await Promise.all([
    fetchAndRenderRisk(),
    fetchAndRenderMemory(),
    fetchAndRenderCausal(),
    fetchAndRenderEvolver(),
  ]);
}

function bindTabActivation() {
  const btn = document.querySelector(TAB_BTN_SELECTOR);
  if (!btn) return;
  btn.addEventListener('click', () => {
    fetchAll(true).catch(() => {});
  });
}

function bindDelegatedHandlers() {
  const root = el('st-tab-dplus');
  if (!root) return;
  root.addEventListener('click', (ev) => {
    const t = ev.target;
    if (!(t instanceof HTMLElement)) return;
    if (t.dataset.refresh) {
      switch (t.dataset.refresh) {
        case 'risk':
          fetchAndRenderRisk();
          return;
        case 'memory':
          fetchAndRenderMemory();
          return;
        case 'causal':
          forceRecomputeCausal();
          return;
        case 'evolver':
          fetchAndRenderEvolver();
          return;
      }
    }
    if (t.dataset.resetStrategy) {
      resetRisk(t.dataset.resetStrategy);
      return;
    }
    if (t.dataset.evolverAction === 'approve' && t.dataset.id) {
      approveProposal(t.dataset.id);
      return;
    }
    if (t.dataset.evolverAction === 'reject' && t.dataset.id) {
      rejectProposal(t.dataset.id);
      return;
    }
  });
}

async function bindEvents() {
  unsubTradeClose = await listen('dplus-trade-close', () => {
    fetchAll(false).catch(() => {});
  });
  unsubCausalUpdate = await listen('causal-graph-update', () => {
    fetchAndRenderCausal().catch(() => {});
  });
  unsubEvolverReport = await listen('evolver-report', () => {
    fetchAndRenderEvolver().catch(() => {});
  });
}

export function initDplus() {
  try {
    bindTabActivation();
    bindDelegatedHandlers();
    bindEvents().catch(() => {});
    fetchAll(true).catch(() => {}); // initial render even before tab opens
  } catch (e) {
    // Module is best-effort; UI continues even if D-PLUS fails to wire.
    /* eslint-disable no-console */
    console.warn('initDplus failed:', e);
  }
}

export function disposeDplus() {
  if (typeof unsubTradeClose === 'function') unsubTradeClose();
  if (typeof unsubCausalUpdate === 'function') unsubCausalUpdate();
  if (typeof unsubEvolverReport === 'function') unsubEvolverReport();
  unsubTradeClose = null;
  unsubCausalUpdate = null;
  unsubEvolverReport = null;
}
