import { escapeHtml as esc } from './esc.js';
import { setBacktestMarkers } from './overlays.js';

import { invoke } from './tauri-bridge.js';

const $id = (id) => document.getElementById(id);
const safeFixed = (v, d = 2) => (v == null || isNaN(v)) ? '—' : v.toFixed(d);

// ─── Marker Conversion ──────────────────────────────────────────────

function toLWCMarkers(btMarkers) {
    return btMarkers.map((m) => ({
        time: m.bar_time,
        position: m.kind === 'Entry' ? 'belowBar' : 'aboveBar',
        color: '#D29922',
        shape: m.kind === 'Entry' ? 'arrowUp' : 'arrowDown',
        text: m.kind === 'Entry' ? 'BT BUY' : 'BT SELL',
        price: m.price,
    }));
}

// ─── Stats Rendering ─────────────────────────────────────────────────

function renderSingleStats(stats, disclaimer) {
    const panel = $id('st-bt-stats');
    if (!panel) return;

    const pnlVal = stats.total_pnl_pct || 0;
    const pnlColor = pnlVal >= 0 ? '#3FB950' : '#F85149';
    const pnlGlow = pnlVal >= 0
        ? 'rgba(63,185,80,0.4)' : 'rgba(248,81,73,0.4)';
    const pfDisplay = stats.profit_factor == null ? '—' : stats.profit_factor === Infinity ? '∞' : safeFixed(stats.profit_factor);

    const disclaimerBanner = disclaimer
        ? `<div style="background:rgba(210,153,34,0.12);border:1px solid rgba(210,153,34,0.3);border-radius:6px;padding:6px 10px;margin-bottom:8px;font-size:11px;color:#D29922;line-height:1.4">${esc(disclaimer)}</div>`
        : '';

    panel.innerHTML = `
        ${disclaimerBanner}
        <div class="st-bt-stats-grid">
            <div class="st-bt-stat">
                <span class="st-bt-stat-label">TIMEFRAME</span>
                <span class="st-bt-stat-value">${esc(stats.timeframe)}</span>
            </div>
            <div class="st-bt-stat">
                <span class="st-bt-stat-label">TRADES</span>
                <span class="st-bt-stat-value">${stats.total_trades}</span>
            </div>
            <div class="st-bt-stat">
                <span class="st-bt-stat-label">WIN%</span>
                <span class="st-bt-stat-value">${safeFixed(stats.win_rate, 1)}%</span>
            </div>
            <div class="st-bt-stat">
                <span class="st-bt-stat-label">AVG GAIN</span>
                <span class="st-bt-stat-value" style="color:#3FB950;text-shadow:0 0 6px rgba(63,185,80,0.4)">${safeFixed(stats.avg_gain_pct)}%</span>
            </div>
            <div class="st-bt-stat">
                <span class="st-bt-stat-label">AVG LOSS</span>
                <span class="st-bt-stat-value" style="color:#F85149;text-shadow:0 0 6px rgba(248,81,73,0.4)">${safeFixed(stats.avg_loss_pct)}%</span>
            </div>
            <div class="st-bt-stat">
                <span class="st-bt-stat-label">PF</span>
                <span class="st-bt-stat-value">${pfDisplay}</span>
            </div>
            <div class="st-bt-stat">
                <span class="st-bt-stat-label">TOTAL PnL</span>
                <span class="st-bt-stat-value" style="color:${pnlColor};text-shadow:0 0 6px ${pnlGlow};font-size:18px">${safeFixed(stats.total_pnl_pct)}%</span>
            </div>
        </div>`;
    panel.style.display = 'block';
}

function renderAllStats(results, disclaimer) {
    const panel = $id('st-bt-stats');
    if (!panel) return;

    let bestIdx = 0;
    let bestPnl = -Infinity;
    results.forEach((r, i) => {
        if (r.stats.total_pnl_pct > bestPnl) {
            bestPnl = r.stats.total_pnl_pct;
            bestIdx = i;
        }
    });

    let rows = '';
    results.forEach((r, i) => {
        const s = r.stats;
        const isBest = i === bestIdx;
        const rowClass = isBest ? ' class="st-bt-row-best"' : '';
        const pnlVal = s.total_pnl_pct || 0;
        const pnlColor = pnlVal >= 0 ? '#3FB950' : '#F85149';
        const pfDisplay = s.profit_factor == null ? '—' : s.profit_factor === Infinity ? '∞' : safeFixed(s.profit_factor);
        rows += `<tr${rowClass}>
            <td>${esc(s.timeframe)}</td>
            <td>${s.total_trades}</td>
            <td>${safeFixed(s.win_rate, 1)}%</td>
            <td style="color:#3FB950">${safeFixed(s.avg_gain_pct)}%</td>
            <td style="color:#F85149">${safeFixed(s.avg_loss_pct)}%</td>
            <td>${pfDisplay}</td>
            <td style="color:${pnlColor};font-weight:700">${safeFixed(s.total_pnl_pct)}%</td>
        </tr>`;
    });

    const disclaimerBanner = disclaimer
        ? `<div style="background:rgba(210,153,34,0.12);border:1px solid rgba(210,153,34,0.3);border-radius:6px;padding:6px 10px;margin-bottom:8px;font-size:11px;color:#D29922;line-height:1.4">${esc(disclaimer)}</div>`
        : '';

    panel.innerHTML = `
        ${disclaimerBanner}
        <table class="st-bt-table">
            <thead>
                <tr>
                    <th>TF</th>
                    <th>TRADES</th>
                    <th>WIN%</th>
                    <th>AVG GAIN</th>
                    <th>AVG LOSS</th>
                    <th>PF</th>
                    <th>TOTAL PnL</th>
                </tr>
            </thead>
            <tbody>${rows}</tbody>
        </table>`;
    panel.style.display = 'block';
}

// ─── Button State ────────────────────────────────────────────────────

function setLoading(btn, loading) {
    if (!btn) return;
    btn.disabled = loading;
    btn.style.opacity = loading ? '0.5' : '1';
}

function getCurrentSymbol() {
    const el = $id('st-header-symbol');
    const sym = el?.textContent?.trim();
    return (sym && sym !== '---') ? sym : '';
}

// ─── Handlers ────────────────────────────────────────────────────────

async function handleRunBT() {
    const symbol = getCurrentSymbol();
    if (!symbol) return;

    const tfSelect = $id('st-bt-tf');
    const timeframe = tfSelect?.value || '5m';
    const btn = $id('st-bt-run');

    setLoading(btn, true);
    try {
        const result = await invoke('run_backtest', { symbol, timeframe });
        setBacktestMarkers(toLWCMarkers(result.markers));
        renderSingleStats(result.stats, result.disclaimer);
    } catch (err) {
        console.error('[backtest] run_backtest failed:', err);
        const panel = $id('st-bt-stats');
        if (panel) {
            panel.innerHTML = `<span style="color:#F85149;font-size:14px">Backtest error: ${esc(String(err))}</span>`;
            panel.style.display = 'block';
        }
    } finally {
        setLoading(btn, false);
    }
}

async function handleRunAll() {
    const symbol = getCurrentSymbol();
    if (!symbol) return;

    const btn = $id('st-bt-run-all');

    setLoading(btn, true);
    try {
        const results = await invoke('run_backtest_all', { symbol });
        setBacktestMarkers([]);
        const firstDisclaimer = results?.[0]?.disclaimer || '';
        renderAllStats(results, firstDisclaimer);
    } catch (err) {
        console.error('[backtest] run_backtest_all failed:', err);
        const panel = $id('st-bt-stats');
        if (panel) {
            panel.innerHTML = `<span style="color:#F85149;font-size:14px">Backtest ALL error: ${esc(String(err))}</span>`;
            panel.style.display = 'block';
        }
    } finally {
        setLoading(btn, false);
    }
}

function handleClear() {
    setBacktestMarkers([]);
    const panel = $id('st-bt-stats');
    if (panel) {
        panel.innerHTML = '';
        panel.style.display = 'none';
    }
}

// ─── Public Init ─────────────────────────────────────────────────────

export function initBacktest() {
    const btnRun = $id('st-bt-run');
    const btnAll = $id('st-bt-run-all');
    const btnClr = $id('st-bt-clear');

    if (btnRun) btnRun.addEventListener('click', handleRunBT);
    if (btnAll) btnAll.addEventListener('click', handleRunAll);
    if (btnClr) btnClr.addEventListener('click', handleClear);
}
