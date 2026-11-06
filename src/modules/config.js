import { on, emit } from './bus.js';
import { showToast } from './toast.js';
import { exportConfig, importConfig } from './config-io.js';

import { invoke, listen } from './tauri-bridge.js';
const RESTART_FIELDS = ['alpaca_key_id', 'alpaca_secret', 'paper_mode', 'sip_feed'];
const $ = (id) => document.getElementById(id);

let _originalConfig = null;
let _importCandidate = null;
let _dirty = false;

const FIELD_MAP = {
    'smarttrend.exit_confirm_bars': 'st-cfg-exit-confirm-bars',
    'smarttrend.risk_per_trade': 'st-cfg-risk-per-trade',
    'smarttrend.max_drawdown_pct': 'st-cfg-max-drawdown',
    'smarttrend.max_hold_minutes': 'st-cfg-max-hold',
    'execution.max_positions': 'st-cfg-max-positions',
    'execution.allow_short': 'st-cfg-allow-short',
    'execution.bracket_atr_mult': 'st-cfg-bracket-atr',
    'execution.order_timeout_secs': 'st-cfg-order-timeout',
    'ui.default_timeframe': 'st-cfg-default-tf',
    'ui.chart_show_volume': 'st-cfg-show-volume',
    'ui.desktop_notifications': 'st-cfg-desktop-notif',
    'ui.sound_on_fill': 'st-cfg-sound-fill',
    'market.alpaca_key_id': 'st-cfg-alpaca-key',
    'market.alpaca_secret': 'st-cfg-alpaca-secret',
    'market.paper_mode': 'st-cfg-paper-mode',
    'market.sip_feed': 'st-cfg-sip-feed',
    'market.discovery_poll_secs': 'st-cfg-discovery-poll',
    'market.stale_data_secs': 'st-cfg-stale-data',
    'market.fmp_api_key': 'st-cfg-fmp-key',
};

function cloneConfig(cfg) {
    return JSON.parse(JSON.stringify(cfg));
}

function activeDraftBase() {
    return _importCandidate || _originalConfig;
}

function nestedVal(obj, path) {
    return path.split('.').reduce((acc, key) => (acc ? acc[key] : undefined), obj);
}

function readNumber(id, fallback) {
    const el = $(id);
    if (!el) return fallback;
    const val = parseFloat(el.value);
    return Number.isFinite(val) ? val : fallback;
}

function readInt(id, fallback) {
    const el = $(id);
    if (!el) return fallback;
    const val = parseInt(el.value, 10);
    return Number.isFinite(val) ? val : fallback;
}

function readBool(id, fallback) {
    const el = $(id);
    return el ? !!el.checked : fallback;
}

function readText(id, fallback = '') {
    const el = $(id);
    return el ? el.value : fallback;
}

function setValue(id, value) {
    const el = $(id);
    if (!el) return;
    el.value = value;
}

function setChecked(id, value) {
    const el = $(id);
    if (!el) return;
    el.checked = !!value;
}

function setText(id, value) {
    const el = $(id);
    if (!el) return;
    el.textContent = value;
}

function updateHoldHint(minutes) {
    const h = Math.floor(minutes / 60);
    const m = minutes % 60;
    setText('st-cfg-max-hold-hint', `${h}h ${m}m`);
}

// Two-state mode indicator — drives the segmented [PAPER][LIVE]
// selector + a hint line. The hidden checkbox `#st-cfg-paper-mode`
// is the form-bound source of truth (Alpaca paper_mode flag); the
// segmented buttons are pure visual + click drivers that flip the
// checkbox and dispatch a `change` event so existing form-dirty
// tracking keeps working.
//
//   paper_mode=true   → PAPER (green, Alpaca paper API — virtual money)
//   paper_mode=false  → LIVE  (red,  Alpaca live  API — real money)
function updatePaperModeIndicator() {
    const paperEl = $('st-cfg-paper-mode');
    const paper = paperEl ? !!paperEl.checked : true;
    const state = paper ? 'paper' : 'live';
    const row = $('st-cfg-paper-row');
    if (row) row.dataset.modeState = state;
    const paperBtn = $('st-cfg-mode-paper-btn');
    const liveBtn = $('st-cfg-mode-live-btn');
    if (paperBtn) {
        paperBtn.classList.toggle('st-cfg-mode-btn--active', paper);
        paperBtn.setAttribute('aria-pressed', paper ? 'true' : 'false');
    }
    if (liveBtn) {
        liveBtn.classList.toggle('st-cfg-mode-btn--active', !paper);
        liveBtn.setAttribute('aria-pressed', paper ? 'false' : 'true');
    }
    const hint = $('st-cfg-mode-hint');
    if (hint) {
        hint.className = 'st-cfg-mode-hint';
        if (paper) {
            hint.textContent = 'PAPER: Alpaca paper API · virtual money · safe for testing';
        } else {
            hint.classList.add('st-cfg-mode-hint--live');
            hint.textContent = 'LIVE: real money at risk · requires Alpaca LIVE API keys above · restart app to apply';
        }
    }
}

// Set the underlying checkbox + fire change event so dirty-tracking
// + indicator refresh stay in sync. Returns true if the value changed.
function setPaperModeChecked(value) {
    const cb = $('st-cfg-paper-mode');
    if (!cb || cb.checked === !!value) return false;
    cb.checked = !!value;
    cb.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
}

// Confirmation modal for the irreversible PAPER → LIVE switch.
// Native confirm() is acceptable here: it blocks until the operator
// either confirms or cancels, can't be dismissed by clicking outside,
// and renders identically across Tauri's webview backends.
function confirmGoLive() {
    return window.confirm(
        '⚠️  SWITCH TO LIVE — REAL MONEY\n\n' +
        'You are about to switch from Alpaca PAPER (virtual money)\n' +
        'to Alpaca LIVE (real money).\n\n' +
        'Before continuing, make sure that:\n' +
        '  • Your Alpaca LIVE API keys are configured in MARKET / BROKER above.\n' +
        '  • The strategy has been validated on PAPER.\n' +
        '  • You accept that any loss will be real.\n\n' +
        'A restart is required for the change to take effect.\n\n' +
        'Are you absolutely sure?'
    );
}

function updateSliderProgress(el) {
    const min = parseFloat(el.min);
    const max = parseFloat(el.max);
    const val = parseFloat(el.value);
    const pct = ((val - min) / (max - min)) * 100;
    el.style.background = `linear-gradient(to right, #58A6FF 0%, #58A6FF ${pct}%, #30363D ${pct}%, #30363D 100%)`;
}

function syncAllSliders() {
    document.querySelectorAll('.st-cfg-slider').forEach(updateSliderProgress);
}

function populateSmarttrend(st) {
    setValue('st-cfg-exit-confirm-bars', st.exit_confirm_bars);
    setValue('st-cfg-risk-per-trade', st.risk_per_trade);
    setText('st-cfg-risk-per-trade-val', `${(st.risk_per_trade * 100).toFixed(1)}%`);
    setValue('st-cfg-max-drawdown', st.max_drawdown_pct);
    setText('st-cfg-max-drawdown-val', `${st.max_drawdown_pct.toFixed(1)}%`);
    setValue('st-cfg-max-hold', st.max_hold_minutes);
    updateHoldHint(st.max_hold_minutes);
}

function populateExecution(execution) {
    setValue('st-cfg-max-positions', execution.max_positions);
    setChecked('st-cfg-allow-short', execution.allow_short);
    setValue('st-cfg-bracket-atr', execution.bracket_atr_mult);
    setText('st-cfg-bracket-atr-val', `${execution.bracket_atr_mult.toFixed(1)}x`);
    setValue('st-cfg-order-timeout', execution.order_timeout_secs);
}

function populateUi(ui) {
    setValue('st-cfg-active-symbol', ui.active_symbol);
    setValue('st-cfg-default-tf', ui.default_timeframe);
    setChecked('st-cfg-show-volume', ui.chart_show_volume);
    setChecked('st-cfg-desktop-notif', ui.desktop_notifications);
    setChecked('st-cfg-sound-fill', ui.sound_on_fill !== false);
}

function setKeyField(id, redactedValue) {
    const el = $(id);
    if (!el) return;
    if (redactedValue === '***') {
        el.value = '';
        el.placeholder = '••• key configured •••';
        el.dataset.redacted = '1';
    } else {
        el.value = '';
        el.placeholder = 'not configured';
        el.dataset.redacted = '0';
    }
}

function populateMarket(market) {
    setKeyField('st-cfg-alpaca-key', market.alpaca_key_id);
    setKeyField('st-cfg-alpaca-secret', market.alpaca_secret);
    setChecked('st-cfg-paper-mode', market.paper_mode);
    setChecked('st-cfg-sip-feed', market.sip_feed);
    setValue('st-cfg-discovery-poll', market.discovery_poll_secs);
    setValue('st-cfg-stale-data', market.stale_data_secs);
    setKeyField('st-cfg-fmp-key', market.fmp_api_key);
}

function populateForm(cfg) {
    if (!$('st-cfg-exit-confirm-bars')) return;
    populateSmarttrend(cfg.smarttrend);
    populateExecution(cfg.execution);
    populateUi(cfg.ui);
    populateMarket(cfg.market);
    syncAllSliders();
    updatePaperModeIndicator();
}

function ensureSections(base) {
    const cfg = base || {};
    cfg.smarttrend = cfg.smarttrend || {};
    cfg.execution = cfg.execution || {};
    cfg.ui = cfg.ui || {};
    cfg.market = cfg.market || {};
    return cfg;
}

function applySmarttrendForm(cfg) {
    const st = cfg.smarttrend;
    st.exit_confirm_bars = readInt('st-cfg-exit-confirm-bars', st.exit_confirm_bars);
    st.risk_per_trade = readNumber('st-cfg-risk-per-trade', st.risk_per_trade);
    st.max_drawdown_pct = readNumber('st-cfg-max-drawdown', st.max_drawdown_pct);
    st.max_hold_minutes = readInt('st-cfg-max-hold', st.max_hold_minutes);
}

function applyExecutionForm(cfg) {
    const execution = cfg.execution;
    execution.max_positions = readInt('st-cfg-max-positions', execution.max_positions);
    execution.allow_short = readBool('st-cfg-allow-short', execution.allow_short);
    execution.bracket_atr_mult = readNumber('st-cfg-bracket-atr', execution.bracket_atr_mult);
    execution.order_timeout_secs = readInt('st-cfg-order-timeout', execution.order_timeout_secs);
}

function applyUiForm(cfg) {
    const ui = cfg.ui;
    ui.active_symbol = readText('st-cfg-active-symbol', ui.active_symbol || 'AAPL').toUpperCase();
    ui.favorites = ui.favorites || ['AAPL', 'MSFT', 'NVDA'];
    ui.default_timeframe = readText('st-cfg-default-tf', ui.default_timeframe || '5m');
    ui.chart_show_volume = readBool('st-cfg-show-volume', ui.chart_show_volume);
    ui.desktop_notifications = readBool('st-cfg-desktop-notif', ui.desktop_notifications);
    ui.sound_on_fill = readBool('st-cfg-sound-fill', ui.sound_on_fill !== false);
}

function readKeyField(id) {
    const el = $(id);
    if (!el) return '***';
    const val = el.value.trim();
    if (val) return val;
    return el.dataset.redacted === '1' ? '***' : '';
}

function applyMarketForm(cfg) {
    const market = cfg.market;
    market.alpaca_key_id = readKeyField('st-cfg-alpaca-key');
    market.alpaca_secret = readKeyField('st-cfg-alpaca-secret');
    market.paper_mode = readBool('st-cfg-paper-mode', market.paper_mode);
    market.sip_feed = readBool('st-cfg-sip-feed', market.sip_feed);
    market.discovery_poll_secs = readInt('st-cfg-discovery-poll', market.discovery_poll_secs);
    market.stale_data_secs = readInt('st-cfg-stale-data', market.stale_data_secs);
    market.fmp_api_key = readKeyField('st-cfg-fmp-key');
}

function buildConfig() {
    const base = activeDraftBase() ? cloneConfig(activeDraftBase()) : {};
    const cfg = ensureSections(base);
    applySmarttrendForm(cfg);
    applyExecutionForm(cfg);
    applyUiForm(cfg);
    applyMarketForm(cfg);
    return cfg;
}

function markDirty() {
    _dirty = true;
    const save = $('st-cfg-save');
    const cancel = $('st-cfg-cancel');
    const indicator = $('st-cfg-dirty');
    if (save) save.disabled = false;
    if (cancel) cancel.disabled = false;
    if (indicator) indicator.textContent = 'Unsaved changes';
    highlightChanges();
}

function clearDirty() {
    _dirty = false;
    const save = $('st-cfg-save');
    const cancel = $('st-cfg-cancel');
    const indicator = $('st-cfg-dirty');
    if (save) save.disabled = true;
    if (cancel) cancel.disabled = true;
    if (indicator) indicator.textContent = '';
    document.querySelectorAll('.st-cfg-row--changed').forEach((row) => {
        row.classList.remove('st-cfg-row--changed');
    });
}

function highlightChanges() {
    if (!_originalConfig) return;
    const current = buildConfig();
    for (const [path, fieldId] of Object.entries(FIELD_MAP)) {
        const row = document.getElementById(fieldId)?.closest('.st-cfg-row');
        if (!row) continue;
        const baseValue = nestedVal(_originalConfig, path);
        const curValue = nestedVal(current, path);
        const changed = typeof baseValue === 'number'
            ? Math.abs(baseValue - curValue) > 1e-9
            : baseValue !== curValue;
        row.classList.toggle('st-cfg-row--changed', changed);
    }
}

function checkRestartRequired(newCfg) {
    if (!_originalConfig) return [];
    return RESTART_FIELDS.filter((field) => newCfg.market[field] !== _originalConfig.market[field]);
}

async function handleSave() {
    const newCfg = buildConfig();
    try {
        await invoke('set_config', { newConfig: newCfg });
        const restartFields = checkRestartRequired(newCfg);
        _originalConfig = cloneConfig(newCfg);
        _importCandidate = null;
        clearDirty();
        showToast('Config saved', 'success');
        if (restartFields.length > 0) {
            showToast(
                `Restart required — Changes to [${restartFields.join(', ')}] take effect after restart`,
                'warning'
            );
        }
        emit('config-saved');
    } catch (e) {
        showToast(`Save failed: ${e}`, 'error');
    }
}

async function handleReset() {
    if (!confirm('Reset all settings to defaults?')) return;
    try {
        const cfg = await invoke('reset_config');
        _originalConfig = cloneConfig(cfg);
        _importCandidate = null;
        populateForm(cfg);
        clearDirty();
        showToast('Config reset to defaults', 'success');
        emit('config-saved');
    } catch (e) {
        showToast(`Reset failed: ${e}`, 'error');
    }
}

function handleCancel() {
    if (!_originalConfig) return;
    _importCandidate = null;
    populateForm(_originalConfig);
    clearDirty();
}

async function loadConfig() {
    try {
        const cfg = await invoke('get_config');
        _originalConfig = cloneConfig(cfg);
        _importCandidate = null;
        populateForm(cfg);
        clearDirty();
    } catch (e) {
        showToast(`Failed to load config: ${e}`, 'error');
    }
}

function handleImportedCandidate(cfg) {
    _importCandidate = cloneConfig(cfg);
    populateForm(_importCandidate);
    markDirty();
}

function setupSectionToggles() {
    document.querySelectorAll('.st-cfg-section-header').forEach((header) => {
        header.addEventListener('click', () => {
            header.closest('.st-cfg-section')?.classList.toggle('st-cfg-section--collapsed');
        });
    });
}

function setupEyeButtons() {
    document.querySelectorAll('.st-cfg-eye-btn').forEach((btn) => {
        btn.addEventListener('click', () => {
            const inputId = btn.getAttribute('data-reveal');
            const input = inputId ? $(inputId) : null;
            if (!input) return;
            input.type = input.type === 'password' ? 'text' : 'password';
        });
    });
}

function setupInputListeners() {
    document.querySelectorAll('.st-cfg-slider').forEach((slider) => {
        slider.addEventListener('input', () => updateSliderProgress(slider));
    });

    const sliderFormatters = [
        ['st-cfg-risk-per-trade', (value) => `${(value * 100).toFixed(1)}%`],
        ['st-cfg-max-drawdown', (value) => `${value.toFixed(1)}%`],
        ['st-cfg-bracket-atr', (value) => `${value.toFixed(1)}x`],
    ];
    sliderFormatters.forEach(([id, formatter]) => {
        $(id)?.addEventListener('input', () => {
            setText(`${id}-val`, formatter(parseFloat($(id).value)));
            markDirty();
        });
    });

    $('st-cfg-max-hold')?.addEventListener('input', () => {
        updateHoldHint(readInt('st-cfg-max-hold', 120));
        markDirty();
    });

    // Trading-mode segmented selector — explicit PAPER/LIVE buttons.
    // The hidden checkbox `#st-cfg-paper-mode` stays the form-bound
    // source of truth; the buttons just drive it. PAPER → LIVE goes
    // through a hard confirmation (real money at stake), the reverse
    // path is single-click + restart.
    $('st-cfg-paper-mode')?.addEventListener('change', updatePaperModeIndicator);
    $('st-cfg-mode-paper-btn')?.addEventListener('click', () => {
        if (setPaperModeChecked(true)) markDirty();
    });
    $('st-cfg-mode-live-btn')?.addEventListener('click', () => {
        const cb = $('st-cfg-paper-mode');
        if (!cb || !cb.checked) return; // already on LIVE
        if (!confirmGoLive()) return;
        if (setPaperModeChecked(false)) markDirty();
    });

    [
        'st-cfg-exit-confirm-bars',
        'st-cfg-max-positions',
        'st-cfg-order-timeout',
        'st-cfg-discovery-poll',
        'st-cfg-stale-data',
        'st-cfg-alpaca-key',
        'st-cfg-alpaca-secret',
        'st-cfg-fmp-key',
        'st-cfg-active-symbol',
    ].forEach((id) => {
        $(id)?.addEventListener('input', markDirty);
    });

    [
        'st-cfg-allow-short',
        'st-cfg-show-volume',
        'st-cfg-desktop-notif',
        'st-cfg-paper-mode',
        'st-cfg-sip-feed',
        'st-cfg-sound-fill',
        'st-cfg-default-tf',
    ].forEach((id) => {
        $(id)?.addEventListener('change', markDirty);
    });

    $('st-cfg-save')?.addEventListener('click', handleSave);
    $('st-cfg-reset')?.addEventListener('click', handleReset);
    $('st-cfg-cancel')?.addEventListener('click', handleCancel);
    $('st-cfg-export')?.addEventListener('click', exportConfig);
    $('st-cfg-import')?.addEventListener('click', importConfig);
}

function setupExternalSync() {
    listen('config-changed', async (event) => {
        if (event.payload?.source === 'set_config') return;
        await loadConfig();
    });
}

export async function initConfig() {
    setupSectionToggles();
    setupEyeButtons();
    setupInputListeners();
    setupExternalSync();

    on('tab-config-active', loadConfig);
    on('config-imported', handleImportedCandidate);
}
