// Strategy Lab — multi-strategy parallel backtest UI.
//
// Wires the LAB tab in `index.html` to the Rust command
// `run_strategy_backtest`. Renders two tables (per-strategy
// leaderboard + per-(strategy, symbol) detail) plus a simple
// SVG sparkline of the best strategy's portfolio equity curve.
//
// Compliance:
//   F1 — ES module, no `window.*` globals (LightweightCharts is NOT
//        used here; equity curve is a hand-rolled SVG to avoid
//        another chart handle).
//   F3 — invoke goes through tauri-bridge.
//   S07 — every dynamic interpolation goes through escapeHtml /
//         numeric formatters; no raw string concat into innerHTML.
//   F4 — every CSS class is prefixed `st-bte-`.

import { escapeHtml as esc } from './esc.js';
import { fmtMoney, fmtNum, fmtPct, fmtPnl } from './fmt.js';
import { invoke } from './tauri-bridge.js';
import { STRATEGIES, strategyLabel } from './strategy-labels.js';

const $id = (id) => document.getElementById(id);

const DEFAULT_SELECTED = new Set([
    'opening_range_breakout',
    'gap_fade',
    'afternoon_fade',
]);

// Timeframes iterated when the operator picks "ALL" — mirrors the
// chart toolbar set and the backend `normalize_alpaca_tf` mapping.
const ALL_TIMEFRAMES = ['1m', '5m', '10m', '15m', '30m', '1h', '4h', '1d'];

// ─── Default date range ─────────────────────────────────────────────

function isoDate(d) {
    return d.toISOString().slice(0, 10);
}

function applyDefaultDates() {
    const startEl = $id('st-bte-start');
    const endEl = $id('st-bte-end');
    if (!startEl || !endEl) return;
    const end = new Date();
    const start = new Date(end.getTime() - 30 * 86400_000);
    if (!startEl.value) startEl.value = isoDate(start);
    if (!endEl.value) endEl.value = isoDate(end);
}

// ─── Strategy chips ─────────────────────────────────────────────────

function renderStrategyChips() {
    const host = $id('st-bte-strategies');
    if (!host) return;
    const chips = STRATEGIES.map(([id, label]) => {
        const checked = DEFAULT_SELECTED.has(id) ? 'checked' : '';
        return `<label class="st-bte-chip">
            <input type="checkbox" class="st-bte-chip-input" value="${esc(id)}" ${checked}>
            <span class="st-bte-chip-label">${esc(label)}</span>
        </label>`;
    }).join('');
    host.innerHTML = chips;
}

function selectedStrategies() {
    return Array.from(
        document.querySelectorAll('.st-bte-chip-input:checked'),
    ).map((el) => el.value);
}

// ─── Form helpers ───────────────────────────────────────────────────

function readSymbols() {
    const raw = $id('st-bte-symbols')?.value || '';
    return raw
        .split(/[\s,]+/)
        .map((s) => s.trim().toUpperCase())
        .filter(Boolean);
}

function readDateAsUtc(id, hour, minute) {
    const v = $id(id)?.value;
    if (!v) return null;
    return new Date(`${v}T${String(hour).padStart(2, '0')}:${String(minute).padStart(2, '0')}:00Z`).toISOString();
}

// ─── Rendering ──────────────────────────────────────────────────────

function pnlClass(v) {
    if (!Number.isFinite(v)) return '';
    return v >= 0 ? 'st-bte-pos' : 'st-bte-neg';
}

function renderStrategyTable(perStrategy) {
    const tbody = $id('st-bte-strategy-tbody');
    if (!tbody) return;
    if (!perStrategy || perStrategy.length === 0) {
        tbody.innerHTML = `<tr><td colspan="13" class="st-bte-empty">No results.</td></tr>`;
        return;
    }
    const rows = perStrategy.map((r) => {
        const m = r.metrics || {};
        const pf = !Number.isFinite(m.profit_factor)
            ? '∞'
            : fmtNum(m.profit_factor, 2);
        return `<tr>
            <td>${esc(strategyLabel(r.strategy))}</td>
            <td>${esc(r.timeframe || '')}</td>
            <td>${esc(String(m.trades ?? 0))}</td>
            <td>${esc(fmtNum(m.win_rate, 1))}%</td>
            <td>${esc(pf)}</td>
            <td class="${pnlClass(m.expectancy)}">${esc(fmtPnl(m.expectancy))}</td>
            <td class="${pnlClass(m.net_pnl)}">${esc(fmtMoney(m.net_pnl))}</td>
            <td>${esc(fmtNum(m.max_drawdown_pct, 2))}%</td>
            <td>${esc(fmtNum(m.sharpe, 2))}</td>
            <td>${esc(fmtNum(m.sortino, 2))}</td>
            <td>${esc(fmtPct(m.cagr, 1))}</td>
            <td>${esc(fmtNum(m.exposure_pct, 1))}%</td>
            <td>${esc(String(r.symbols_traded ?? 0))}</td>
        </tr>`;
    }).join('');
    tbody.innerHTML = rows;
}

function renderSymbolTable(perSymbol) {
    const tbody = $id('st-bte-symbol-tbody');
    if (!tbody) return;
    if (!perSymbol || perSymbol.length === 0) {
        tbody.innerHTML = `<tr><td colspan="10" class="st-bte-empty">No results.</td></tr>`;
        return;
    }
    // Sort by net PnL descending for at-a-glance ranking.
    const sorted = [...perSymbol].sort((a, b) => {
        const an = a.metrics?.net_pnl ?? 0;
        const bn = b.metrics?.net_pnl ?? 0;
        return bn - an;
    });
    const rows = sorted.map((r) => {
        const m = r.metrics || {};
        const pf = !Number.isFinite(m.profit_factor)
            ? '∞'
            : fmtNum(m.profit_factor, 2);
        return `<tr>
            <td>${esc(strategyLabel(r.strategy))}</td>
            <td>${esc(r.timeframe || '')}</td>
            <td>${esc(r.symbol)}</td>
            <td>${esc(String(m.trades ?? 0))}</td>
            <td>${esc(fmtNum(m.win_rate, 1))}%</td>
            <td>${esc(pf)}</td>
            <td class="${pnlClass(m.net_pnl)}">${esc(fmtMoney(m.net_pnl))}</td>
            <td>${esc(fmtNum(m.max_drawdown_pct, 2))}%</td>
            <td>${esc(fmtNum(m.sharpe, 2))}</td>
            <td>${esc(fmtMoney(m.final_equity))}</td>
        </tr>`;
    }).join('');
    tbody.innerHTML = rows;
}

// SVG sparkline of the leading strategy's equity curve. Kept tiny on
// purpose — avoids a second LightweightCharts handle in the same UI.
function renderEquityCurve(perStrategy) {
    const host = $id('st-bte-equity-wrap');
    if (!host) return;
    const top = perStrategy?.[0];
    const curve = top?.equity_curve;
    if (!curve || curve.length < 2) {
        host.innerHTML = '';
        return;
    }
    const width = 720;
    const height = 160;
    const padX = 8;
    const padY = 12;
    const values = curve.map((p) => p.equity);
    const minV = Math.min(...values);
    const maxV = Math.max(...values);
    const span = Math.max(maxV - minV, 1e-9);
    const stepX = (width - 2 * padX) / Math.max(curve.length - 1, 1);
    const points = curve
        .map((p, i) => {
            const x = padX + i * stepX;
            const y = height - padY - ((p.equity - minV) / span) * (height - 2 * padY);
            return `${x.toFixed(1)},${y.toFixed(1)}`;
        })
        .join(' ');
    host.innerHTML = `
        <header class="st-bte-section-header">
            <span class="st-bte-section-title">EQUITY CURVE — ${esc(strategyLabel(top.strategy))}</span>
            <span class="st-bte-section-meta">${esc(fmtMoney(minV))} → ${esc(fmtMoney(maxV))}</span>
        </header>
        <svg class="st-bte-spark" viewBox="0 0 ${width} ${height}" preserveAspectRatio="none">
            <polyline class="st-bte-spark-line" points="${points}"></polyline>
        </svg>`;
}

// ─── Handler ────────────────────────────────────────────────────────

function setStatus(msg, kind = 'info') {
    const el = $id('st-bte-status');
    if (!el) return;
    el.className = `st-bte-status st-bte-status--${kind}`;
    el.textContent = msg;
}

function setRunning(running) {
    const btn = $id('st-bte-run');
    if (!btn) return;
    btn.disabled = running;
    btn.textContent = running ? 'RUNNING…' : 'RUN BACKTEST';
}

// Tags every per-strategy / per-symbol row with the timeframe it was run
// under, so an "ALL" sweep stays readable in the combined tables.
function tagRowsWithTf(report, tf) {
    (report.per_strategy || []).forEach((r) => { r.timeframe = tf; });
    (report.per_symbol || []).forEach((r) => { r.timeframe = tf; });
    return report;
}

async function runOneTimeframe(tf, baseArgs) {
    const report = await invoke('run_strategy_backtest', {
        args: { ...baseArgs, timeframe: tf },
    });
    return tagRowsWithTf(report, tf);
}

async function runBacktest() {
    const symbols = readSymbols();
    const strategies = selectedStrategies();
    const tf = $id('st-bte-tf')?.value || '5m';
    const start = readDateAsUtc('st-bte-start', 13, 30); // 09:30 ET ≈ 13:30 UTC (EDT)
    const end = readDateAsUtc('st-bte-end', 20, 0);     // 16:00 ET ≈ 20:00 UTC (EDT)
    const initialEquity = parseFloat($id('st-bte-equity')?.value || '10000');
    const slippageBps = parseFloat($id('st-bte-slippage')?.value || '5');

    if (symbols.length === 0) {
        setStatus('Add at least one symbol.', 'error');
        return;
    }
    if (strategies.length === 0) {
        setStatus('Select at least one strategy.', 'error');
        return;
    }
    if (!start || !end) {
        setStatus('Provide a start and end date.', 'error');
        return;
    }

    const baseArgs = {
        symbols,
        strategies,
        start,
        end,
        initial_equity: initialEquity,
        slippage_bps: slippageBps,
        commission: 0,
        spread_bps: 5,
        use_cache: true,
    };
    const sweep = tf === 'all';
    const timeframes = sweep ? ALL_TIMEFRAMES : [tf];

    setRunning(true);

    try {
        const perStrategy = [];
        const perSymbol = [];
        let simulateMs = 0;
        let symbolsFetched = 0;
        let symbolsFailed = 0;
        let disclaimerText = '';

        for (let i = 0; i < timeframes.length; i += 1) {
            const cur = timeframes[i];
            setStatus(
                sweep
                    ? `Simulating ${cur} (${i + 1}/${timeframes.length})…`
                    : 'Fetching candles and simulating…',
                'info',
            );
            // eslint-disable-next-line no-await-in-loop
            const report = await runOneTimeframe(cur, baseArgs);
            perStrategy.push(...(report.per_strategy || []));
            perSymbol.push(...(report.per_symbol || []));
            simulateMs += report.simulate_ms || 0;
            symbolsFetched = Math.max(symbolsFetched, report.symbols_fetched || 0);
            symbolsFailed += report.symbols_failed?.length || 0;
            disclaimerText = report.disclaimer || disclaimerText;
        }

        // Best strategy first so the equity sparkline shows the leader.
        perStrategy.sort((a, b) => (b.metrics?.net_pnl ?? 0) - (a.metrics?.net_pnl ?? 0));

        const disclaimer = $id('st-bte-disclaimer');
        if (disclaimer) disclaimer.textContent = disclaimerText;
        renderStrategyTable(perStrategy);
        renderSymbolTable(perSymbol);
        renderEquityCurve(perStrategy);

        const failedMsg = symbolsFailed > 0
            ? ` (${symbolsFailed} skipped)`
            : '';
        const tfMsg = sweep ? ` across ${timeframes.length} timeframes` : '';
        setStatus(
            `Done — ${symbolsFetched} symbol${symbolsFetched === 1 ? '' : 's'} ` +
                `× ${strategies.length} strateg${strategies.length === 1 ? 'y' : 'ies'}${tfMsg} ` +
                `in ${simulateMs} ms simulate${failedMsg}.`,
            'ok',
        );
    } catch (err) {
        // eslint-disable-next-line no-console
        console.error('[backtest-engine] run_strategy_backtest failed:', err);
        setStatus(`Error: ${String(err)}`, 'error');
    } finally {
        setRunning(false);
    }
}

// ─── Public init ────────────────────────────────────────────────────

export function initBacktestEngine() {
    renderStrategyChips();
    applyDefaultDates();
    const btn = $id('st-bte-run');
    if (btn) btn.addEventListener('click', runBacktest);
}
