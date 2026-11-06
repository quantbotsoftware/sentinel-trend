import { emit } from './bus.js';
import { showToast } from './toast.js';

import { invoke, listen } from './tauri-bridge.js';

const $ = (id) => document.getElementById(id);

// ─── Presets (EXACT from plan) ──────────────────────────────────────

const DEFAULT_SCAN_PARAMS = {
    price_min: 1,
    price_max: 500,
    volume_min: 500_000,
    change_min: 1,
    rvol_min: 1,
    gap_min: 0,
    float_max: 0,
    cap_max: 0,
    exclude_earnings: false,
    max_candidates: 100,
    scan_interval_minutes: 30,
    score_preset: 'balanced',
};

const PRESETS = {
    ross_cameron: {
        price_min: 1, price_max: 20, volume_min: 1_000_000, change_min: 10,
        rvol_min: 5, gap_min: 0, float_max: 20_000_000, cap_max: 2_000_000_000,
        exclude_earnings: false, max_candidates: 100, scan_interval_minutes: 30, score_preset: 'balanced',
    },
    penny_rockets: {
        price_min: 0.1, price_max: 5, volume_min: 2_000_000, change_min: 20,
        rvol_min: 5, gap_min: 0, float_max: 0, cap_max: 0,
        exclude_earnings: false, max_candidates: 50, scan_interval_minutes: 30, score_preset: 'momentum',
    },
    swing_setup: {
        price_min: 8, price_max: 150, volume_min: 1_500_000, change_min: 2,
        rvol_min: 1.8, gap_min: 0.5, float_max: 0, cap_max: 0,
        exclude_earnings: true, max_candidates: 60, scan_interval_minutes: 30, score_preset: 'gap_scanner',
    },
    large_cap: {
        price_min: 50, price_max: 500, volume_min: 5_000_000, change_min: 3,
        rvol_min: 1, gap_min: 0, float_max: 0, cap_max: 0,
        exclude_earnings: false, max_candidates: 50, scan_interval_minutes: 30, score_preset: 'volume_hunter',
    },
    hot_momentum: {
        price_min: 1, price_max: 50, volume_min: 1_000_000, change_min: 10,
        rvol_min: 3, gap_min: 0, float_max: 0, cap_max: 0,
        exclude_earnings: false, max_candidates: 100, scan_interval_minutes: 30, score_preset: 'momentum',
    },
    // Premarket catalyst hunters: lower absolute volume floor, require
    // a real gap/move so news-backed explosions surface.
    catalyst_premarket: {
        price_min: 1, price_max: 80, volume_min: 75_000, change_min: 5,
        rvol_min: 2, gap_min: 2, float_max: 0, cap_max: 0,
        exclude_earnings: false, max_candidates: 60, scan_interval_minutes: 10, score_preset: 'gap_scanner',
    },
    // Premarket Ignition: names exploding pre-bell. $-volume floor,
    // spread cap and alert threshold live in `ignition` (backend
    // `scanner::premarket`); gap_min is ignored by this profile.
    premarket_ignition: {
        price_min: 1, price_max: 100, volume_min: 0, change_min: 8,
        rvol_min: 3, gap_min: 0, float_max: 0, cap_max: 0,
        exclude_earnings: false, max_candidates: 40, scan_interval_minutes: 15,
        scan_interval_premarket_minutes: 2, score_preset: 'premarket_ignition',
        ignition: { min_dollar_volume: 300_000, max_spread_bps: 200, require_catalyst: false, alert_min_score: 55, alert_max_per_scan: 5 },
    },
};

let _autoScanActive = false;
let _activePreset = '';
// Fields the form does not expose (premarket cadence, rvol_method,
// ignition knobs). Carried through `readScanParams` so saving from the
// form never resets them to backend defaults.
let _hiddenParams = {};

function setText(id, text) {
    const el = $(id);
    if (el) el.textContent = text;
}

function normalizeScanParams(sp = {}) {
    return { ...DEFAULT_SCAN_PARAMS, ...sp };
}

// ─── Slider Progress Fill ───────────────────────────────────────────

function updateSliderFill(el) {
    const min = parseFloat(el.min);
    const max = parseFloat(el.max);
    const val = parseFloat(el.value);
    if (!Number.isFinite(min) || !Number.isFinite(max) || max <= min) return;
    const pct = ((val - min) / (max - min)) * 100;
    el.style.background = `linear-gradient(to right, #58A6FF 0%, #58A6FF ${pct}%, #30363D ${pct}%, #30363D 100%)`;
}

function syncSliderDisplays() {
    const changeSlider = $('st-scan-change-min');
    if (changeSlider) {
        setText('st-scan-change-val', `${changeSlider.value}%`);
        updateSliderFill(changeSlider);
    }

    const candSlider = $('st-scan-max-candidates');
    if (candSlider) {
        setText('st-scan-max-cand-val', candSlider.value.toString());
        updateSliderFill(candSlider);
    }

    const intervalSlider = $('st-scan-interval');
    if (intervalSlider) {
        setText('st-scan-interval-val', `${intervalSlider.value}m`);
        updateSliderFill(intervalSlider);
    }
}

function syncVolumeHint() {
    const volInput = $('st-scan-volume-min');
    if (volInput) {
        setText('st-scan-vol-hint', formatVol(parseFloat(volInput.value) || 0));
    }
}

function syncAllScanSliders() {
    syncSliderDisplays();
    syncVolumeHint();
}

// ─── Volume Format ──────────────────────────────────────────────────

function formatVol(val) {
    if (val >= 1_000_000) return (val / 1_000_000).toFixed(1) + 'M';
    if (val >= 1_000) return (val / 1_000).toFixed(0) + 'K';
    return val.toString();
}

// ─── Read Form -> ScanParams ────────────────────────────────────────

function readScanParams() {
    const interval = parseInt($('st-scan-interval')?.value || '30', 10);
    return {
        ..._hiddenParams,
        price_min: parseFloat($('st-scan-price-min')?.value || '1'),
        price_max: parseFloat($('st-scan-price-max')?.value || '500'),
        volume_min: parseFloat($('st-scan-volume-min')?.value || '500000'),
        change_min: parseFloat($('st-scan-change-min')?.value || '1'),
        rvol_min: parseFloat($('st-scan-rvol-min')?.value || '1'),
        gap_min: parseFloat($('st-scan-gap-min')?.value || '0'),
        float_max: parseFloat($('st-scan-float-max')?.value || '0'),
        cap_max: parseFloat($('st-scan-cap-max')?.value || '0'),
        exclude_earnings: $('st-scan-exclude-earnings')?.checked || false,
        max_candidates: parseInt($('st-scan-max-candidates')?.value || '100', 10),
        scan_interval_minutes: interval,
        score_preset: $('st-scan-score-preset')?.value || 'balanced',
    };
}

// ─── Apply Preset to Form ───────────────────────────────────────────

function setActivePreset(preset) {
    _activePreset = preset || '';
    $('st-scan-btn-ross')?.classList.toggle('st-scan-btn-ross--active', _activePreset === 'ross_cameron');
    document.querySelectorAll('.st-scan-preset-btn').forEach((btn) => {
        btn.classList.toggle('st-scan-preset-btn--active', btn.dataset.preset === _activePreset);
    });
}

function clearActivePreset() {
    if (_activePreset) setActivePreset('');
}

function applyPreset(preset) {
    const p = PRESETS[preset];
    if (!p) return;

    populateFromConfig(p);
    setActivePreset(preset);
    const label = preset.replace(/_/g, ' ').toUpperCase();
    updateStatus(`Preset applied: ${label}`);
    showToast(`Preset applied: ${label}`, 'success');
}

// ─── Populate Form from Config ──────────────────────────────────────

function populateFromConfig(sp) {
    const params = normalizeScanParams(sp);
    const { scan_interval_premarket_minutes, rvol_method, ignition } = params;
    _hiddenParams = {
        ..._hiddenParams,
        ...(scan_interval_premarket_minutes !== undefined && { scan_interval_premarket_minutes }),
        ...(rvol_method !== undefined && { rvol_method }),
        ...(ignition !== undefined && { ignition }),
    };
    $('st-scan-price-min').value = params.price_min;
    $('st-scan-price-max').value = params.price_max;
    $('st-scan-volume-min').value = params.volume_min;
    $('st-scan-change-min').value = params.change_min;
    $('st-scan-rvol-min').value = params.rvol_min;
    $('st-scan-gap-min').value = params.gap_min;
    $('st-scan-float-max').value = params.float_max;
    $('st-scan-cap-max').value = params.cap_max;
    $('st-scan-exclude-earnings').checked = params.exclude_earnings;
    $('st-scan-max-candidates').value = params.max_candidates;
    $('st-scan-interval').value = params.scan_interval_minutes;
    $('st-scan-score-preset').value = params.score_preset;
    syncAllScanSliders();
}

// ─── Status Update ──────────────────────────────────────────────────

function updateStatus(msg) {
    const el = $('st-scan-status');
    if (el) el.textContent = msg;
}

function updateProgress(progress) {
    const percent = Math.max(0, Math.min(100, Number(progress?.percent) || 0));
    const wrap = $('st-scan-progress');
    const fill = $('st-scan-progress-fill');
    if (wrap) {
        wrap.classList.toggle('st-scan-progress--error', progress?.phase === 'error');
        wrap.classList.toggle('st-scan-progress--complete', progress?.phase === 'complete');
    }
    if (fill) {
        fill.style.width = `${percent}%`;
    }
    setText('st-scan-progress-label', progress?.label || 'Ready to scan');
    setText('st-scan-progress-pct', `${percent}%`);
}

function updateAutoScanButton(active) {
    _autoScanActive = active;
    const btn = $('st-scan-btn-auto');
    if (!btn) return;
    if (active) {
        btn.textContent = 'AUTO SCAN: ON';
        btn.classList.add('st-scan-btn-auto--on');
    } else {
        btn.textContent = 'AUTO SCAN: OFF';
        btn.classList.remove('st-scan-btn-auto--on');
    }
}

// ─── Wire Buttons ───────────────────────────────────────────────────

function wireScanButton() {
    const btn = $('st-scan-btn-scan');
    if (!btn) return;

    btn.addEventListener('click', async () => {
        const params = readScanParams();
        updateProgress({ phase: 'start', label: 'Starting scan...', percent: 5 });
        updateStatus('Scanning...');
        btn.disabled = true;
        try {
            const results = await invoke('run_market_scan', { params });
            const count = results ? results.length : 0;
            updateProgress({ phase: 'complete', label: `Scan complete: ${count} results`, percent: 100 });
            updateStatus(`${count} results found`);
        } catch (err) {
            updateProgress({ phase: 'error', label: 'Scan failed', percent: 100 });
            updateStatus('Scan failed');
            showToast(`Scan error: ${err}`, 'error');
        } finally {
            btn.disabled = false;
        }
    });
}

function wireClearButton() {
    const btn = $('st-scan-btn-clear');
    if (!btn) return;

    btn.addEventListener('click', () => {
        populateFromConfig(DEFAULT_SCAN_PARAMS);
        setActivePreset('');
        updateStatus('Filters cleared');
        updateProgress({ phase: 'idle', label: 'Ready to scan', percent: 0 });
        showToast('Scanner filters reset to defaults', 'success');
    });
}

function wireAutoScan() {
    const btn = $('st-scan-btn-auto');
    if (!btn) return;

    btn.addEventListener('click', async () => {
        try {
            const params = readScanParams();
            await invoke('set_scan_params', { params });
            const msg = await invoke('toggle_auto_scan');
            showToast(msg, 'success');
        } catch (err) {
            showToast(`Auto-scan toggle failed: ${err}`, 'error');
        }
    });
}

function wirePresets() {
    $('st-scan-btn-ross')?.addEventListener('click', (event) => {
        event.preventDefault();
        applyPreset('ross_cameron');
    });

    document.querySelectorAll('.st-scan-preset-btn').forEach((btn) => {
        btn.addEventListener('click', (event) => {
            event.preventDefault();
            const preset = btn.dataset.preset;
            if (preset) applyPreset(preset);
        });
    });
}

function wireScoreMode() {
    const sel = $('st-scan-score-preset');
    if (!sel) return;
    sel.addEventListener('change', () => {
        clearActivePreset();
        updateStatus(`Score mode: ${sel.value.replace(/_/g, ' ')}`);
    });
}

function bindFieldEvents(el, handler) {
    if (!el) return;
    ['input', 'change'].forEach((eventName) => {
        el.addEventListener(eventName, handler);
    });
}

function wireSliders() {
    const changeSlider = $('st-scan-change-min');
    bindFieldEvents(changeSlider, () => {
        clearActivePreset();
        syncSliderDisplays();
    });

    const candSlider = $('st-scan-max-candidates');
    bindFieldEvents(candSlider, () => {
        clearActivePreset();
        syncSliderDisplays();
    });

    const intervalSlider = $('st-scan-interval');
    bindFieldEvents(intervalSlider, () => {
        clearActivePreset();
        syncSliderDisplays();
    });

    const volInput = $('st-scan-volume-min');
    bindFieldEvents(volInput, () => {
        clearActivePreset();
        syncVolumeHint();
    });

    [
        'st-scan-price-min',
        'st-scan-price-max',
        'st-scan-rvol-min',
        'st-scan-gap-min',
        'st-scan-float-max',
        'st-scan-cap-max',
    ].forEach((id) => bindFieldEvents($(id), clearActivePreset));

    $('st-scan-exclude-earnings')?.addEventListener('change', clearActivePreset);
}

// ─── Apply Scan to AutoUniverse Watchlist ───────────────────────────
//
// S18.1 — single-click promote/demote pipeline. Calls the same Rust
// helper as the auto-scan loop (`apply_scan_to_universe`), so the
// rules are identical: score ≥ min_promote_score, rvol ≥ min_promote_rvol,
// stale entries (>60min unseen) demoted, blacklist respected, open
// positions / FSM-active symbols protected from eviction.
function wireInjectButton() {
    const btn = $('st-scan-btn-inject');
    if (!btn) return;
    btn.addEventListener('click', async () => {
        btn.disabled = true;
        try {
            const report = await invoke('apply_scan_to_watchlist');
            const promoted = (report?.promoted || []).length;
            const demoted = (report?.demoted || []).length;
            const skipped = (report?.skipped_blacklisted || []).length;
            if (promoted === 0 && demoted === 0) {
                showToast('Watchlist already aligned with the latest scan — no changes', 'info');
            } else {
                const parts = [];
                if (promoted > 0) parts.push(`+${promoted} promoted`);
                if (demoted > 0) parts.push(`-${demoted} demoted`);
                if (skipped > 0) parts.push(`${skipped} blacklisted`);
                showToast(`Watchlist updated: ${parts.join(' · ')}`, 'success');
            }
            emit('watchlist-changed', { source: 'scanner-apply', promoted, demoted });
        } catch (err) {
            const msg = String(err);
            if (msg.includes('No scan results')) {
                showToast('Run a scan first, then click APPLY → WATCHLIST', 'warning');
            } else if (msg.includes('AutoUniverse is disabled')) {
                showToast('Enable AutoUniverse in CONFIG first', 'warning');
            } else {
                showToast(`Apply failed: ${err}`, 'error');
            }
        } finally { btn.disabled = false; }
    });
}

// ─── Tauri Event Listeners ──────────────────────────────────────────

function setupTauriListeners() {
    listen('scan-progress', (event) => {
        const progress = event.payload;
        if (!progress) return;
        updateProgress(progress);
        if (progress.label) updateStatus(progress.label);
    });

    listen('scan-results', (event) => {
        const results = event.payload || [];
        if (_autoScanActive) {
            updateStatus(`Auto: ${results.length} results`);
        }
    });

    listen('scan-status', (event) => {
        const status = event.payload;
        if (!status) return;
        updateAutoScanButton(status.auto_active);
        if (status.running) {
            updateStatus('Scanning...');
        } else if (status.last_scan_ts) {
            const d = new Date(status.last_scan_ts * 1000);
            const hm = d.toLocaleTimeString('en-US', { hour: '2-digit', minute: '2-digit', hour12: false });
            const dur = status.last_scan_duration_ms ? ` in ${status.last_scan_duration_ms}ms` : '';
            updateStatus(`Last: ${hm} — ${status.results_count} results${dur}`);
        }
    });
}

// ─── Public Init ────────────────────────────────────────────────────

export async function initScanner() {
    wireScanButton();
    wireClearButton();
    wireAutoScan();
    wireInjectButton();
    wirePresets();
    wireScoreMode();
    wireSliders();
    setupTauriListeners();

    try {
        const cfg = await invoke('get_config');
        if (cfg?.market?.scan_params) {
            populateFromConfig(cfg.market.scan_params);
        }
        updateAutoScanButton(cfg?.market?.auto_scan_active || false);
    } catch (err) {
        console.error('[scanner] Failed to load config:', err);
    }

    updateProgress({ phase: 'idle', label: 'Ready to scan', percent: 0 });
    syncAllScanSliders();
}
