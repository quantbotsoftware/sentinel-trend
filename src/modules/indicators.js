// Overlays on the main price chart: EMA (9, 20, 50, 200), VWAP, Bollinger Bands (20, 2.0)
// Pure-JS math so no backend roundtrip. Candle data arrives via bus events from chart.js.
// Each overlay is individually toggleable, persisted in localStorage.
//
// F1 exception: LightweightCharts (`LWC`) is a global because its script is loaded via
// <script> tag. Same reasoning as chart.js; an ES module build of LWC is a separate task.

import { on } from './bus.js';
import { getMainChart } from './chart.js';

const LWC = window.LightweightCharts;

const VIS = {
    ema: 'st-ema-visible',
    ema50: 'st-ema50-visible',
    ema200: 'st-ema200-visible',
    vwap: 'st-vwap-visible',
    bb: 'st-bb-visible',
};

function readVis(k) { return localStorage.getItem(k) !== 'false'; }

const vis = {
    ema: readVis(VIS.ema),
    ema50: readVis(VIS.ema50),
    ema200: readVis(VIS.ema200),
    vwap: readVis(VIS.vwap),
    bb: readVis(VIS.bb),
};

let ema9Series = null;
let ema20Series = null;
let ema50Series = null;
let ema200Series = null;
let vwapSeries = null;
let bbUpperSeries = null;
let bbMidSeries = null;
let bbLowerSeries = null;

let _candles = [];

// ─── Pure-JS Math ────────────────────────────────────────────────────

export function calcEma(closes, period) {
    const alpha = 2 / (period + 1);
    const out = new Array(closes.length);
    let prev = null;
    for (let i = 0; i < closes.length; i++) {
        prev = prev === null ? closes[i] : closes[i] * alpha + prev * (1 - alpha);
        out[i] = prev;
    }
    return out;
}

export function calcVwap(candles) {
    const out = new Array(candles.length);
    let cumV = 0, cumVP = 0;
    for (let i = 0; i < candles.length; i++) {
        const c = candles[i];
        const tp = (c.high + c.low + c.close) / 3;
        const v = c.volume || 0;
        cumV += v;
        cumVP += v * tp;
        out[i] = cumV === 0 ? tp : cumVP / cumV;
    }
    return out;
}

export function calcBB(closes, period = 20, mult = 2.0) {
    const n = closes.length;
    const upper = new Array(n);
    const middle = new Array(n);
    const lower = new Array(n);
    for (let i = 0; i < n; i++) {
        if (i < period - 1) { upper[i] = middle[i] = lower[i] = null; continue; }
        let sum = 0;
        for (let j = i - period + 1; j <= i; j++) sum += closes[j];
        const mean = sum / period;
        let sq = 0;
        for (let j = i - period + 1; j <= i; j++) sq += (closes[j] - mean) ** 2;
        const std = Math.sqrt(sq / period);
        middle[i] = mean;
        upper[i] = mean + mult * std;
        lower[i] = mean - mult * std;
    }
    return { upper, middle, lower };
}

// ─── Series creation ─────────────────────────────────────────────────

function mkLine(chart, color, width, style, visible) {
    return chart.addLineSeries({
        color,
        lineWidth: width,
        lineStyle: style,
        priceScaleId: 'right',
        priceLineVisible: false,
        lastValueVisible: false,
        visible,
    });
}

function createOverlaySeries() {
    const chart = getMainChart();
    if (!chart) return;

    ema9Series   = mkLine(chart, '#FFD700',           2, LWC.LineStyle.Solid,  vis.ema);
    ema20Series  = mkLine(chart, '#FF4444',           2, LWC.LineStyle.Solid,  vis.ema);
    ema50Series  = mkLine(chart, '#58A6FF',           2, LWC.LineStyle.Solid,  vis.ema50);
    ema200Series = mkLine(chart, '#8B4513',           2, LWC.LineStyle.Solid,  vis.ema200);
    vwapSeries   = mkLine(chart, '#FF00FF',           2, LWC.LineStyle.Dotted, vis.vwap);
    bbUpperSeries = mkLine(chart, 'rgba(124,181,236,0.85)', 1, LWC.LineStyle.Solid, vis.bb);
    bbMidSeries   = mkLine(chart, 'rgba(200,200,200,0.75)', 1, LWC.LineStyle.Solid, vis.bb);
    bbLowerSeries = mkLine(chart, 'rgba(124,181,236,0.85)', 1, LWC.LineStyle.Solid, vis.bb);
}

function zip(times, values) {
    const pts = [];
    for (let i = 0; i < times.length; i++) {
        if (values[i] != null && Number.isFinite(values[i])) {
            pts.push({ time: times[i], value: values[i] });
        }
    }
    return pts;
}

// ─── Compute & apply ─────────────────────────────────────────────────

function applyOverlays(candles) {
    _candles = Array.isArray(candles) ? candles : [];
    if (_candles.length === 0) { clearAll(); return; }

    const closes = _candles.map(c => c.close);
    const times  = _candles.map(c => c.time);

    const ema9   = calcEma(closes, 9);
    const ema20  = calcEma(closes, 20);
    const ema50  = calcEma(closes, 50);
    const ema200 = calcEma(closes, 200);
    const vwap   = calcVwap(_candles);
    const bb     = calcBB(closes);

    ema9Series?.setData(zip(times, ema9));
    ema20Series?.setData(zip(times, ema20));
    ema50Series?.setData(zip(times, ema50));
    ema200Series?.setData(zip(times, ema200));
    vwapSeries?.setData(zip(times, vwap));
    bbUpperSeries?.setData(zip(times, bb.upper));
    bbMidSeries?.setData(zip(times, bb.middle));
    bbLowerSeries?.setData(zip(times, bb.lower));
}

function clearAll() {
    const all = [
        ema9Series, ema20Series, ema50Series, ema200Series,
        vwapSeries, bbUpperSeries, bbMidSeries, bbLowerSeries,
    ];
    all.forEach(s => { if (s) try { s.setData([]); } catch (_) { /* noop */ } });
}

// ─── Toggle wiring ───────────────────────────────────────────────────

function bindToggle(btnId, lsKey, stateKey, onChange) {
    const btn = document.getElementById(btnId);
    if (!btn) return;
    btn.classList.toggle('st-tf-btn--active', vis[stateKey]);
    btn.addEventListener('click', () => {
        vis[stateKey] = !vis[stateKey];
        localStorage.setItem(lsKey, String(vis[stateKey]));
        btn.classList.toggle('st-tf-btn--active', vis[stateKey]);
        onChange(vis[stateKey]);
    });
}

function initToggles() {
    bindToggle('st-ema-toggle', VIS.ema, 'ema', v => {
        ema9Series?.applyOptions({ visible: v });
        ema20Series?.applyOptions({ visible: v });
    });
    bindToggle('st-ema50-toggle', VIS.ema50, 'ema50',
        v => ema50Series?.applyOptions({ visible: v }));
    bindToggle('st-ema200-toggle', VIS.ema200, 'ema200',
        v => ema200Series?.applyOptions({ visible: v }));
    bindToggle('st-vwap-toggle', VIS.vwap, 'vwap',
        v => vwapSeries?.applyOptions({ visible: v }));
    bindToggle('st-bb-toggle', VIS.bb, 'bb', v => {
        bbUpperSeries?.applyOptions({ visible: v });
        bbMidSeries?.applyOptions({ visible: v });
        bbLowerSeries?.applyOptions({ visible: v });
    });
}

// ─── Bus listeners ───────────────────────────────────────────────────

function setupBusListeners() {
    on('chart-candles', (data) => {
        if (data?.candles) applyOverlays(data.candles);
    });
    // Clear overlay series the instant the user switches symbol so stale
    // EMA/VWAP/BB values from the previous symbol don't pin the right
    // price-scale autoscale and hide new candles below the viewport.
    on('chart-clear', () => {
        _candles = [];
        clearAll();
    });
}

// ─── Public ──────────────────────────────────────────────────────────

export function getIndicatorCandles() { return _candles; }

export function initIndicators() {
    createOverlaySeries();
    initToggles();
    setupBusListeners();
}

export function disposeIndicators() {
    _candles = [];
    ema9Series = null; ema20Series = null; ema50Series = null; ema200Series = null;
    vwapSeries = null; bbUpperSeries = null; bbMidSeries = null; bbLowerSeries = null;
}
