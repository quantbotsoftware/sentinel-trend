import { summarizeActionableAlert } from './alert-fmt.js';
import { emit } from './bus.js';
import { initChart, disposeChart } from './chart.js';
import { initOverlays, disposeOverlays } from './overlays.js';
import { initIndicators, disposeIndicators } from './indicators.js';
import { initPanes, disposePanes } from './indicators-panes.js';
import { initPositions, disposePositions } from './positions.js';
import { showToast } from './toast.js';
import { initKeyboard } from './keyboard.js';
import { initSmartTrend } from './smarttrend.js';
import { initDiscovery, disposeDiscovery } from './discovery/index.js';
import { initJournal } from './journal.js';
import { initEquity, disposeEquity } from './equity.js';
import { initConfig } from './config.js';
import { initScanner } from './scanner.js';
import { initBacktest } from './backtest.js';
import { initBacktestEngine } from './backtest-engine.js';
import { initTournament, disposeTournament } from './tournament.js';
import { initDplus, disposeDplus } from './dplus.js';
import { initOmega, disposeOmega } from './omega.js';
import { initPrime } from './prime.js';
import { toggleTradingMode } from './strategy.js';
import { initTradeability } from './tradeability.js';
import { initTradePlan } from './trade-plan.js';
import { initApexRadar } from './apex-radar.js';
import { initGoAlert } from './go-alert.js';
import { initMeridianPanel } from './meridian-panel.js';
import { initMeridianTab } from './meridian-tab.js';
import { initScannerIgnition } from './scanner-ignition.js';
import { initFrontierPanel } from './frontier-panel.js';
import { initBarCountdown } from './bar-countdown.js';

import { fmtNum } from './fmt.js';
import { invoke, listen, _disposeAllListeners } from './tauri-bridge.js';

const $id = (id) => document.getElementById(id);

let _clockTimer = null;
let _healthTimer = null;

// ─── Build Info ──────────────────────────────────────────────────────

async function loadBuildInfo() {
    try {
        const info = await invoke('get_build_info');
        $id('st-version').textContent = `v${info.version}`;
        $id('st-hash').textContent = info.git_hash;
        $id('st-date').textContent = info.build_date;
    } catch (err) {
        console.error('Failed to load build info:', err);
    }
}

// ─── Health Polling ──────────────────────────────────────────────────

async function loadHealth() {
    try {
        const h = await invoke('get_health');

        const modeEl = $id('st-mode');
        if (modeEl) {
            modeEl.textContent = h.trading_mode;
            modeEl.classList.toggle('st-sidebar-mode-btn--auto', h.trading_mode === 'Auto');
        }
        const pmEl = $id('st-paper-mode');
        if (pmEl) {
            // Two-state label: PAPER (Alpaca paper API, virtual money) or
            // LIVE (real money). DRY mode was removed in v2.5.
            pmEl.textContent = h.paper_mode ? 'PAPER' : 'LIVE';
        }
        $id('st-session').textContent = h.session_id.substring(0, 8);

        const symEl = $id('st-symbol');
        if (symEl && h.active_symbol) {
            symEl.textContent = h.active_symbol;
        }

        const dot = $id('st-health-dot');
        dot.classList.remove('hud-health-dot--green', 'hud-health-dot--red');
        dot.classList.add('hud-health-dot--green');
        $id('st-status').textContent = 'READY';

        updateUptime(h.uptime_secs);

        const breaker = $id('st-breaker');
        breaker.className = 'hud-value';
        if (h.breaker_level === 'Normal') {
            breaker.classList.add('hud-value--green');
        } else if (h.breaker_level === 'Caution') {
            breaker.classList.add('hud-value--gold');
        } else {
            breaker.classList.add('hud-value--red');
        }
        breaker.textContent = h.breaker_level;

        const paperEl = $id('st-paper-mode');
        if (paperEl) {
            paperEl.className = 'hud-value';
            // Two-state colour: PAPER=green (safe virtual money) or
            // LIVE=red (real money — operator must explicitly opt in).
            paperEl.classList.add(h.paper_mode ? 'hud-value--green' : 'hud-value--red');
        }

        const streamDot = $id('st-stream-dot');
        const streamStatus = $id('st-stream-status');
        streamDot.classList.remove('hud-health-dot--green', 'hud-health-dot--red');
        if (h.stream_connected) {
            streamDot.classList.add('hud-health-dot--green');
            streamStatus.textContent = 'CONNECTED';
        } else {
            streamDot.classList.add('hud-health-dot--red');
            streamStatus.textContent = 'OFFLINE';
        }
    } catch (err) {
        console.error('Health check failed:', err);
        const dot = $id('st-health-dot');
        dot.classList.remove('hud-health-dot--green');
        dot.classList.add('hud-health-dot--red');
        $id('st-status').textContent = 'ERROR';
    }
}

function updateUptime(secs) {
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    const s = secs % 60;
    $id('st-uptime').textContent =
        `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
}

// ─── Stream Listeners (sidebar) ──────────────────────────────────────

function setupStreamListeners() {
    listen('price-update', (event) => {
        const { symbol, price } = event.payload;
        const priceEl = $id('st-stream-price');
        if (priceEl) {
            priceEl.textContent = `${symbol} $${fmtNum(price)}`;

        }
    });

    listen('symbol-changed', (event) => {
        const sym = event.payload;
        const symEl = $id('st-symbol');
        if (symEl && sym) {
            symEl.textContent = sym;
        }
    });

    listen('stream-status', (event) => {
        const streamDot = $id('st-stream-dot');
        const streamStatus = $id('st-stream-status');
        streamDot.classList.remove('hud-health-dot--green', 'hud-health-dot--red');
        if (event.payload === 'CONNECTED') {
            streamDot.classList.add('hud-health-dot--green');
            streamStatus.textContent = 'CONNECTED';
        } else {
            streamDot.classList.add('hud-health-dot--red');
            streamStatus.textContent = 'OFFLINE';
        }
    });
}

// ─── S02 Compliance Status Bar (PDT counter + warning) ───────────────
//
// Listens to the same `equity-sync` event consumed by positions.js
// (updateWallet) and projects the compliance subset onto status-bar rows
// added in S02. Kept in core.js to respect S02 scope discipline (don't
// touch positions.js).

function setupComplianceListeners() {
    listen('equity-sync', (event) => {
        const p = event.payload;
        if (!p) return;

        const dtEl = $id('st-day-trades');
        if (dtEl && Number.isInteger(p.daytrade_count)) {
            dtEl.textContent = `${p.daytrade_count}`;
            dtEl.classList.remove('hud-value--red', 'hud-value--gold');
            if (p.equity_below_25k && p.daytrade_count >= 3) {
                dtEl.classList.add('hud-value--red');
            } else if (p.equity_below_25k && p.daytrade_count === 2) {
                dtEl.classList.add('hud-value--gold');
            }
        }

        const warnEl = $id('st-pdt-warning');
        if (warnEl) {
            const showWarn = !!(p.equity_below_25k &&
                (p.daytrade_count >= 2 || p.pattern_day_trader));
            warnEl.hidden = !showWarn;
        }
    });

    // Surface bot-blocked compliance events as toast warnings (helps the
    // operator notice over-blocking quickly during paper testing).
    listen('bot-blocked', (event) => {
        const p = event.payload;
        if (!p || !p.reason) return;
        // Only show toasts for compliance gates introduced in S02 (not the
        // pre-existing "Max positions" / "Position already open" reasons,
        // which are already shown in the bot status panel).
        const compliance = ['PDT_BLOCKED', 'BUYING_POWER_INSUFFICIENT',
            'SHORT_DISABLED', 'NOT_SHORTABLE', 'NOT_FRACTIONABLE',
            'ASSET_UNKNOWN', 'ACCOUNT_STALE'];
        if (!compliance.includes(p.reason)) return;
        // Reuse the existing toast mechanism via emit (loose coupling).
        const symbol = p.symbol || '?';
        const message = `BOT: ${symbol} blocked — ${p.reason.replace(/_/g, ' ').toLowerCase()}`;
        emit('toast-warning', { message });
    });
}

// ─── Sentinel Listeners (sidebar FSM display) ────────────────────────

const FSM_COLORS = {
    Idle: 'st-fsm--idle',
    Stalking: 'st-fsm--stalking',
    Confirming: 'st-fsm--confirming',
    Active: 'st-fsm--active',
    Exit: 'st-fsm--exit',
    Pullback: 'st-fsm--pullback',
};

function setupSentinelListeners() {
    listen('smarttrend-eval', (event) => {
        const payload = event.payload;
        const activeSymbol = $id('st-header-symbol')?.textContent?.trim();
        if (payload?.symbol && activeSymbol && payload.symbol !== activeSymbol) return;
        const d = payload && payload.diagnostics;
        if (!d) return;

        const scoreEl = $id('st-sentinel-score');
        if (scoreEl) {
            const s = d.combined_score;
            scoreEl.textContent = fmtNum(s, 1);
            scoreEl.className = 'hud-value hud-value--mono';
            if (s >= 70) scoreEl.classList.add('hud-value--green');
            else if (s >= 50) scoreEl.classList.add('hud-value--cyan');
        }

        const dirEl = $id('st-sentinel-dir');
        if (dirEl) {
            dirEl.textContent = d.combined_direction || '---';
            dirEl.className = 'hud-value hud-value--mono';
            if (d.combined_direction === 'Bullish') dirEl.classList.add('hud-value--green');
            else if (d.combined_direction === 'Bearish') dirEl.classList.add('hud-value--red');
        }

        const strEl = $id('st-sentinel-strength');
        if (strEl) {
            strEl.textContent = d.strength || '---';
        }
    });

    listen('smarttrend-alert', (event) => {
        const a = event.payload;
        if (!a) return;
        const activeSymbol = $id('st-header-symbol')?.textContent?.trim();
        if (a.symbol && activeSymbol && a.symbol !== activeSymbol) return;

        const phaseEl = $id('st-sentinel-phase');
        if (phaseEl) {
            phaseEl.textContent = a.phase || '---';
            phaseEl.className = 'hud-value hud-value--mono st-fsm-phase';
            const cls = FSM_COLORS[a.phase];
            if (cls) phaseEl.classList.add(cls);

            const transition =
                a.prev_phase && a.phase && a.prev_phase !== a.phase
                    ? `${a.prev_phase} -> ${a.phase}`
                    : (a.phase || '');
            const markerSummary = summarizeActionableAlert(a.markers);
            const details = [
                transition ? `Phase: ${transition}` : '',
                a.direction ? `Dir: ${a.direction}` : '',
                Number.isFinite(a.score) ? `Score: ${fmtNum(a.score, 1)}` : '',
                markerSummary ? `Action: ${markerSummary}` : '',
            ].filter(Boolean);
            phaseEl.title = details.join(' | ');
        }
    });

    listen('sentinel-status', (event) => {
        const m = event.payload;
        if (!m) return;

        const statusEl = $id('st-sentinel-status');
        if (statusEl) {
            statusEl.textContent = 'ACTIVE';
            statusEl.className = 'hud-value hud-value--mono hud-value--green';
        }

        const evalsEl = $id('st-sentinel-evals');
        if (evalsEl) {
            evalsEl.textContent = String(m.evals_total || 0);
        }
    });
}

// ─── NY Clock ────────────────────────────────────────────────────────

const MARKET_STATES = {
    PREMARKET:  { label: 'PRE',   css: 'st-ny-clock--premarket' },
    OPEN:       { label: 'OPEN',  css: 'st-ny-clock--open' },
    CLOSING:    { label: 'CLOSE', css: 'st-ny-clock--closing' },
    AFTERHOURS: { label: 'AH',    css: 'st-ny-clock--afterhours' },
    CLOSED:     { label: 'CLOSED', css: 'st-ny-clock--closed' },
    WEEKEND:    { label: 'WKND',  css: 'st-ny-clock--weekend' },
};

function getMarketState(nyDate) {
    const day = nyDate.getDay();
    if (day === 0 || day === 6) return MARKET_STATES.WEEKEND;

    const h = nyDate.getHours();
    const m = nyDate.getMinutes();
    const mins = h * 60 + m;

    if (mins < 240) return MARKET_STATES.CLOSED;       // 00:00-03:59
    if (mins < 570) return MARKET_STATES.PREMARKET;     // 04:00-09:29
    if (mins < 955) return MARKET_STATES.OPEN;          // 09:30-15:54
    if (mins <= 960) return MARKET_STATES.CLOSING;      // 15:55-16:00
    if (mins <= 1200) return MARKET_STATES.AFTERHOURS;  // 16:01-20:00
    return MARKET_STATES.CLOSED;                        // 20:01-23:59
}

function updateNYClock() {
    const el = $id('st-ny-clock');
    if (!el) return;

    const nyStr = new Date().toLocaleString('en-US', {
        timeZone: 'America/New_York',
        hour12: false,
        year: 'numeric', month: '2-digit', day: '2-digit',
        hour: '2-digit', minute: '2-digit', second: '2-digit',
    });

    const nyDate = new Date(nyStr);
    const hh = String(nyDate.getHours()).padStart(2, '0');
    const mm = String(nyDate.getMinutes()).padStart(2, '0');
    const ss = String(nyDate.getSeconds()).padStart(2, '0');

    const state = getMarketState(nyDate);
    el.textContent = `NY ${hh}:${mm}:${ss} ${state.label}`;

    el.className = 'st-ny-clock';
    el.classList.add(state.css);
}

// ─── Bot Event Listeners (toast notifications) ──────────────────────

function setupBotListeners() {
    listen('bot-order', (event) => {
        const p = event.payload;
        if (p) showToast(`BOT ORDER: ${p.side || ''} ${p.symbol || ''} x${p.qty || ''}`, 'success');
    });

    listen('bot-exit', (event) => {
        const p = event.payload;
        if (p) showToast(`BOT EXIT: ${p.symbol || ''} (${p.reason || ''})`, 'warning');
    });

    listen('bot-blocked', (event) => {
        const p = event.payload;
        if (p) showToast(`BOT BLOCKED: ${p.symbol || ''} — ${p.reason || ''}`, 'warning');
    });

    listen('toast-warning', (event) => {
        const p = event.payload;
        if (p && p.message) showToast(p.message, 'warning');
    });

}

// ─── Risk + Watchdog Listeners ───────────────────────────────────────

function setupRiskListeners() {
    listen('breaker-changed', (event) => {
        const p = event.payload;
        if (!p) return;
        const breaker = $id('st-breaker');
        if (breaker) {
            breaker.textContent = p.new;
            breaker.className = 'hud-value';
            if (p.new === 'Normal') breaker.classList.add('hud-value--green');
            else if (p.new === 'Caution') breaker.classList.add('hud-value--gold');
            else breaker.classList.add('hud-value--red');
        }
        if (p.new !== 'Normal') {
            showToast(`BREAKER: ${p.prev} → ${p.new}`, p.new === 'Panic' ? 'error' : 'warning');
        }
    });

    listen('daily-reset', (event) => {
        const p = event.payload;
        if (p) showToast(`Daily reset at ${p.time_et || ''} ET`, 'success');
    });

    listen('watchdog-alert', (event) => {
        const p = event.payload;
        if (p) showToast(`WATCHDOG: ${p.loop_name} ${p.severity} (${p.stale_secs}s)`, 'warning');
    });
}

// ─── Symbol Quick-Switch (click header symbol -> inline input) ───────

function setupSymbolInput() {
    const symEl = $id('st-header-symbol');
    if (!symEl) return;

    symEl.classList.add('st-header-symbol--clickable');

    symEl.addEventListener('click', () => {
        if (symEl.querySelector('input')) return;

        const current = symEl.textContent.trim();
        const input = document.createElement('input');
        input.type = 'text';
        input.className = 'st-symbol-input';
        input.value = current;
        input.maxLength = 8;

        symEl.textContent = '';
        symEl.appendChild(input);
        input.focus();
        input.select();

        const commit = async () => {
            const val = input.value.trim().toUpperCase();
            input.remove();
            if (val && val !== current) {
                symEl.textContent = val;
                try {
                    await invoke('set_active_symbol', { symbol: val });
                    emit('symbol-changed', { symbol: val });
                    showToast(`Symbol → ${val}`, 'success');
                } catch (err) {
                    symEl.textContent = current;
                    showToast(`Invalid symbol: ${err}`, 'error');
                }
            } else {
                symEl.textContent = current;
            }
        };

        input.addEventListener('keydown', (e) => {
            if (e.key === 'Enter') { e.preventDefault(); commit(); }
            if (e.key === 'Escape') { e.preventDefault(); input.remove(); symEl.textContent = current; }
        });
        input.addEventListener('blur', commit);
    });
}

async function applyDefaultTimeframeFromConfig() {
    try {
        const cfg = await invoke('get_config');
        const tf = cfg?.ui?.default_timeframe;
        if (!tf || typeof tf !== 'string') return;

        const btn = document.querySelector(`.st-tf-btn[data-tf="${tf}"]`);
        if (!btn) return;
        if (btn.classList.contains('st-tf-btn--active')) return;

        btn.click();
    } catch (err) {
        console.warn('[core] failed to apply default timeframe:', err);
    }
}

// ─── Init ────────────────────────────────────────────────────────────

async function safeInitStep(label, fn) {
    try {
        await fn();
    } catch (err) {
        console.error(`[core] ${label} init failed:`, err);
    }
}

async function init() {
    await loadBuildInfo();
    await loadHealth();
    setupStreamListeners();
    setupSentinelListeners();
    setupBotListeners();
    setupRiskListeners();
    setupComplianceListeners();
    setupSymbolInput();

    const modeEl = $id('st-mode');
    if (modeEl) {
        modeEl.addEventListener('click', () => {
            toggleTradingMode({
                onResult: (result) => {
                    modeEl.textContent = result;
                    modeEl.classList.toggle('st-sidebar-mode-btn--auto', result === 'Auto');
                },
            });
        });
    }

    await safeInitStep('scanner', initScanner);
    await safeInitStep('chart', initChart);
    await safeInitStep('overlays', initOverlays);
    await safeInitStep('indicators', async () => initIndicators());
    await safeInitStep('indicator panes', async () => initPanes());
    safeInitStep('backtest', initBacktest);
    safeInitStep('backtest-engine', initBacktestEngine);
    await safeInitStep('default timeframe', applyDefaultTimeframeFromConfig);
    safeInitStep('bar-countdown', async () => initBarCountdown());
    await safeInitStep('positions', initPositions);
    safeInitStep('keyboard', async () => initKeyboard());
    safeInitStep('smarttrend', async () => initSmartTrend());
    await safeInitStep('discovery', initDiscovery);
    await safeInitStep('journal', initJournal);
    safeInitStep('tournament', async () => initTournament());
    safeInitStep('dplus', async () => initDplus());
    safeInitStep('omega', async () => initOmega());
    safeInitStep('prime', async () => initPrime());
    safeInitStep('tradeability', async () => initTradeability());
    safeInitStep('trade-plan', async () => initTradePlan());
    safeInitStep('apex-radar', async () => initApexRadar());
    safeInitStep('go-alert', initGoAlert);
    safeInitStep('scanner-ignition', initScannerIgnition);
    safeInitStep('meridian-panel', initMeridianPanel);
    safeInitStep('meridian-tab', initMeridianTab);
    safeInitStep('frontier-panel', async () => initFrontierPanel());
    await safeInitStep('config', initConfig);
    await safeInitStep('equity', initEquity);

    updateNYClock();
    _clockTimer = setInterval(updateNYClock, 1000);
    _healthTimer = setInterval(loadHealth, 5000);
}

function disposeAll() {
    if (_clockTimer) { clearInterval(_clockTimer); _clockTimer = null; }
    if (_healthTimer) { clearInterval(_healthTimer); _healthTimer = null; }
    disposePositions();
    disposeDiscovery();
    disposePanes();
    disposeIndicators();
    disposeChart();
    disposeEquity();
    disposeOverlays();
    disposeTournament();
    disposeDplus();
    disposeOmega();
    _disposeAllListeners();
}

document.addEventListener('DOMContentLoaded', () => {
    init().catch((err) => {
        console.error('[core] init crashed:', err);
        showToast(`Init failed: ${err}`, 'error');
    });
});

window.addEventListener('beforeunload', disposeAll);
