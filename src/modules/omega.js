// OMEGA (Sesiunea 3) — Frontend module.
//
// Wires the four sub-panels (Model status, Live predictions, Regime
// forecast, Re-train controls) inside the new "OMEGA" tab. Listens
// on push events from the backend (`omega-train-complete`,
// `omega-train-failed`) for low-latency refreshes; re-fetches on
// tab activation as a safety net.
//
// All commands return data shaped to match
// `src-tauri/src/commands/omega.rs`.

import { invoke, listen } from './tauri-bridge.js';
import { showToast } from './toast.js';

const TAB_BTN_SELECTOR = '.st-tab-btn[data-tab="omega"]';

const TRAIN_LOG_MAX = 5;

let unsubTrainComplete = null;
let unsubTrainFailed = null;
const trainLog = [];

function el(id) {
  return document.getElementById(id);
}

function escape(str) {
  if (typeof str !== 'string') return '';
  return str.replace(/[&<>"']/g, (c) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  })[c] || c);
}

function fmtTs(ts) {
  if (!ts || ts <= 0) return 'never';
  return new Date(ts * 1000).toLocaleString();
}

function fmtAge(ts) {
  if (!ts || ts <= 0) return '—';
  const ageS = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (ageS < 60) return `${ageS}s`;
  if (ageS < 3600) return `${Math.floor(ageS / 60)}m`;
  return `${Math.floor(ageS / 3600)}h`;
}

function fmtPct(v) {
  if (typeof v !== 'number' || !isFinite(v)) return '—';
  return `${(v * 100).toFixed(1)}%`;
}

function dirLabel(d) {
  if (typeof d !== 'number' || !isFinite(d)) return '—';
  if (d > 0.05) return `+${d.toFixed(2)}`;
  if (d < -0.05) return d.toFixed(2);
  return '0.00';
}

function dirClass(d) {
  if (typeof d !== 'number' || !isFinite(d)) return '';
  if (d > 0.05) return 'omega-dir--positive';
  if (d < -0.05) return 'omega-dir--negative';
  return '';
}

function confidenceBar(c) {
  const pct = Math.max(0, Math.min(1, c)) * 100;
  return `<div class="omega-conf-bar"><div class="omega-conf-fill" style="width:${pct.toFixed(1)}%"></div><span class="omega-conf-pct">${pct.toFixed(0)}%</span></div>`;
}

// ════════════════════════════════════════════════════════════════════
//  Model status
// ════════════════════════════════════════════════════════════════════

async function fetchAndRenderModels() {
  const body = el('omega-models-body');
  const empty = el('omega-models-empty');
  const actions = el('omega-train-actions');
  if (!body || !empty || !actions) return;
  try {
    const out = await invoke('get_omega_state');
    body.innerHTML = '';
    actions.innerHTML = '';
    if (!out || !Array.isArray(out.models) || out.models.length === 0) {
      empty.style.display = '';
      return;
    }
    empty.style.display = 'none';
    for (const m of out.models) {
      const tr = document.createElement('tr');
      const stateClass = m.is_warm ? 'omega-state--warm' : 'omega-state--cold';
      const stateLabel = m.is_warm ? 'WARM' : `COLD (need ${out.cold_start_threshold - m.samples_trained})`;
      tr.innerHTML = `
        <td>${escape(m.model_id)}</td>
        <td>${escape(m.architecture)}</td>
        <td class="num">${m.version}</td>
        <td class="num">${fmtPct(m.accuracy)}</td>
        <td class="num">${m.samples_trained.toLocaleString()}</td>
        <td><span class="${stateClass}">${stateLabel}</span></td>
        <td>
          <label class="omega-toggle"><input type="checkbox" data-toggle-model="${escape(m.model_id)}" ${m.enabled ? 'checked' : ''}/> ENABLED</label>
        </td>
      `;
      body.appendChild(tr);

      const actionRow = document.createElement('div');
      actionRow.className = 'omega-train-action-row';
      actionRow.innerHTML = `
        <span class="omega-train-action-name">${escape(m.model_id)}</span>
        <span class="omega-train-action-meta">last trained: ${fmtTs(m.last_trained_at)}</span>
        <button class="omega-action-btn" data-force-retrain="${escape(m.model_id)}">FORCE RETRAIN</button>
      `;
      actions.appendChild(actionRow);
    }
  } catch (e) {
    empty.style.display = '';
    empty.textContent = `Failed to load OMEGA state: ${e}`;
  }
}

// ════════════════════════════════════════════════════════════════════
//  Live predictions
// ════════════════════════════════════════════════════════════════════

async function fetchAndRenderPredictions() {
  const body = el('omega-pred-body');
  const empty = el('omega-pred-empty');
  if (!body || !empty) return;
  try {
    const rows = await invoke('get_recent_predictions', { limit: 20 });
    body.innerHTML = '';
    if (!Array.isArray(rows) || rows.length === 0) {
      empty.style.display = '';
      return;
    }
    empty.style.display = 'none';
    for (const r of rows.slice(0, 10)) {
      const tr = document.createElement('tr');
      tr.innerHTML = `
        <td>${escape(r.symbol)}</td>
        <td>${escape(r.model_id)}</td>
        <td class="num ${dirClass(r.direction)}">${dirLabel(r.direction)}</td>
        <td>${confidenceBar(r.confidence)}</td>
        <td class="num">${fmtAge(r.created_at)}</td>
      `;
      body.appendChild(tr);
    }
  } catch (e) {
    empty.style.display = '';
    empty.textContent = `Failed to load predictions: ${e}`;
  }
}

// ════════════════════════════════════════════════════════════════════
//  Regime forecast
// ════════════════════════════════════════════════════════════════════

async function fetchAndRenderRegime() {
  const root = el('omega-regime-bars');
  const horizon = el('omega-regime-horizon');
  if (!root) return;
  try {
    const f = await invoke('get_regime_forecast');
    if (!f || !Array.isArray(f.probabilities)) {
      root.innerHTML = '<div class="omega-empty">No regime forecast available</div>';
      return;
    }
    root.innerHTML = '';
    const labels = f.labels || ['calm', 'trending', 'volatile', 'panic'];
    f.probabilities.forEach((p, i) => {
      const pct = Math.max(0, Math.min(1, p)) * 100;
      const isMax = i === f.argmax_index;
      const div = document.createElement('div');
      div.className = `omega-regime-row${isMax ? ' omega-regime-row--max' : ''}`;
      div.innerHTML = `
        <span class="omega-regime-label">${escape(labels[i] || '?')}</span>
        <div class="omega-regime-bar"><div class="omega-regime-fill" style="width:${pct.toFixed(1)}%"></div></div>
        <span class="omega-regime-pct">${pct.toFixed(1)}%</span>
      `;
      root.appendChild(div);
    });
    if (horizon) {
      horizon.textContent = f.is_warm
        ? `${f.horizon_minutes || 90} min`
        : 'cold-start (uniform)';
    }
  } catch (e) {
    root.innerHTML = `<div class="omega-empty">${escape(String(e))}</div>`;
  }
}

// ════════════════════════════════════════════════════════════════════
//  Train log
// ════════════════════════════════════════════════════════════════════

function appendTrainLog(entry) {
  trainLog.unshift({ ts: Math.floor(Date.now() / 1000), ...entry });
  while (trainLog.length > TRAIN_LOG_MAX) trainLog.pop();
  renderTrainLog();
}

function renderTrainLog() {
  const root = el('omega-train-log');
  if (!root) return;
  if (trainLog.length === 0) {
    root.innerHTML = '<div class="omega-empty">No training events yet</div>';
    return;
  }
  root.innerHTML = trainLog
    .map((entry) => {
      const cls = entry.kind === 'failed' ? 'omega-log-row--failed' : 'omega-log-row--ok';
      const detail = entry.kind === 'failed'
        ? escape(entry.message || 'unknown error')
        : `v${entry.version} acc:${fmtPct(entry.accuracy)} n:${entry.samples}`;
      return `<div class="omega-log-row ${cls}">
        <span class="omega-log-ts">${fmtTs(entry.ts)}</span>
        <span class="omega-log-model">${escape(entry.model_id || '?')}</span>
        <span class="omega-log-detail">${detail}</span>
      </div>`;
    })
    .join('');
}

// ════════════════════════════════════════════════════════════════════
//  Actions
// ════════════════════════════════════════════════════════════════════

async function forceRetrain(modelId) {
  if (!modelId) return;
  try {
    await invoke('force_retrain', { modelId });
    showToast(`OMEGA: retrain queued for ${modelId}`, 'info');
  } catch (e) {
    showToast(`Force retrain failed: ${e}`, 'error');
  }
}

async function toggleModel(modelId, enabled) {
  if (!modelId) return;
  try {
    await invoke('toggle_omega_enabled', { modelId, enabled });
  } catch (e) {
    showToast(`Toggle failed: ${e}`, 'error');
  }
}

// ════════════════════════════════════════════════════════════════════
//  Lifecycle
// ════════════════════════════════════════════════════════════════════

async function fetchAll() {
  await Promise.allSettled([
    fetchAndRenderModels(),
    fetchAndRenderPredictions(),
    fetchAndRenderRegime(),
  ]);
  renderTrainLog();
}

function bindTabActivation() {
  const btn = document.querySelector(TAB_BTN_SELECTOR);
  if (!btn) return;
  btn.addEventListener('click', () => {
    fetchAll().catch(() => {});
  });
}

function bindDelegatedHandlers() {
  const root = el('st-tab-omega');
  if (!root) return;
  root.addEventListener('click', (ev) => {
    const t = ev.target;
    if (!(t instanceof HTMLElement)) return;
    if (t.dataset.refresh) {
      switch (t.dataset.refresh) {
        case 'models':
          fetchAndRenderModels();
          return;
        case 'predictions':
          fetchAndRenderPredictions();
          return;
        case 'regime':
          fetchAndRenderRegime();
          return;
        case 'train':
          renderTrainLog();
          return;
      }
    }
    if (t.dataset.forceRetrain) {
      forceRetrain(t.dataset.forceRetrain);
      return;
    }
  });
  root.addEventListener('change', (ev) => {
    const t = ev.target;
    if (!(t instanceof HTMLInputElement)) return;
    if (t.dataset.toggleModel) {
      toggleModel(t.dataset.toggleModel, t.checked);
    }
  });
}

async function bindEvents() {
  unsubTrainComplete = await listen('omega-train-complete', (ev) => {
    appendTrainLog({ kind: 'ok', ...(ev?.payload || {}) });
    fetchAll().catch(() => {});
  });
  unsubTrainFailed = await listen('omega-train-failed', (ev) => {
    appendTrainLog({ kind: 'failed', ...(ev?.payload || {}) });
  });
}

export function initOmega() {
  try {
    bindTabActivation();
    bindDelegatedHandlers();
    bindEvents().catch(() => {});
    fetchAll().catch(() => {});
  } catch (e) {
    /* eslint-disable no-console */
    console.warn('initOmega failed:', e);
  }
}

export function disposeOmega() {
  if (typeof unsubTrainComplete === 'function') unsubTrainComplete();
  if (typeof unsubTrainFailed === 'function') unsubTrainFailed();
  unsubTrainComplete = null;
  unsubTrainFailed = null;
}
