// Oscillator pane (MACD / RSI / ADX / StochRSI) with tab-based selection,
// pane visibility toggle, drag divider, and time-scale sync to the main chart.
// Shares candle data via the `chart-candles` bus event (same as indicators.js).
//
// F1 exception: LightweightCharts is a global (see chart.js/indicators.js for rationale).

import { on, emit } from './bus.js';
import { getMainChart } from './chart.js';
import { calcMacd, calcRsi, calcAdx, calcStochRsi } from './indicators-osc-math.js';

const LWC = window.LightweightCharts;

const VIS_KEY = 'st-osc-visible';
const ACTIVE_KEY = 'st-osc-active';
const DEFAULT_OSC = 'macd';

let oscChart = null;
let _visible = localStorage.getItem(VIS_KEY) !== 'false';
let _active = localStorage.getItem(ACTIVE_KEY) || DEFAULT_OSC;
let _candles = [];

const series = {
    macd: { hist: null, line: null, signal: null },
    rsi: { line: null, ob: null, os: null },
    adx: { line: null },
    stochrsi: { line: null, ob: null, os: null },
};

const OSC_OPTS = {
    layout: {
        background: { type: 'solid', color: '#0D1117' },
        textColor: '#8B949E',
        fontFamily: "'Courier New', Consolas, monospace",
        fontSize: 14,
    },
    grid: {
        vertLines: { color: 'rgba(48, 54, 61, 0.4)' },
        horzLines: { color: 'rgba(48, 54, 61, 0.4)' },
    },
    timeScale: { visible: false },
    handleScroll: false,
    handleScale: false,
    rightPriceScale: {
        borderColor: 'rgba(48, 54, 61, 0.8)',
        scaleMargins: { top: 0.1, bottom: 0.1 },
        // Match main chart + volume pane (82) so MACD histogram
        // bars line up vertically with candles + volume bars.
        minimumWidth: 82,
    },
    crosshair: {
        mode: LWC.CrosshairMode.Normal,
        vertLine: { color: 'rgba(88, 166, 255, 0.4)', width: 1, style: LWC.LineStyle.Dashed },
        horzLine: { color: 'rgba(88, 166, 255, 0.4)', width: 1, style: LWC.LineStyle.Dashed },
    },
};

function createOscPane() {
    const container = document.getElementById('st-chart-osc');
    if (!container) return;

    oscChart = LWC.createChart(container, {
        ...OSC_OPTS,
        width: container.clientWidth,
        height: container.clientHeight,
    });

    series.macd.hist = oscChart.addHistogramSeries({
        priceFormat: { type: 'price', precision: 4, minMove: 0.0001 },
        priceLineVisible: false, lastValueVisible: false, base: 0,
    });
    series.macd.line = oscChart.addLineSeries({
        color: '#58A6FF', lineWidth: 1,
        priceLineVisible: false, lastValueVisible: false,
    });
    series.macd.signal = oscChart.addLineSeries({
        color: '#F0883E', lineWidth: 1,
        priceLineVisible: false, lastValueVisible: false,
    });

    series.rsi.line = oscChart.addLineSeries({
        color: '#58A6FF', lineWidth: 1,
        priceLineVisible: false, lastValueVisible: false,
    });
    series.rsi.ob = oscChart.addLineSeries({
        color: 'rgba(248,81,73,0.35)', lineWidth: 1, lineStyle: LWC.LineStyle.Dashed,
        priceLineVisible: false, lastValueVisible: false,
    });
    series.rsi.os = oscChart.addLineSeries({
        color: 'rgba(63,185,80,0.35)', lineWidth: 1, lineStyle: LWC.LineStyle.Dashed,
        priceLineVisible: false, lastValueVisible: false,
    });

    series.adx.line = oscChart.addLineSeries({
        color: '#F0883E', lineWidth: 2,
        priceLineVisible: false, lastValueVisible: false,
    });

    series.stochrsi.line = oscChart.addLineSeries({
        color: '#58A6FF', lineWidth: 1,
        priceLineVisible: false, lastValueVisible: false,
    });
    series.stochrsi.ob = oscChart.addLineSeries({
        color: 'rgba(248,81,73,0.35)', lineWidth: 1, lineStyle: LWC.LineStyle.Dashed,
        priceLineVisible: false, lastValueVisible: false,
    });
    series.stochrsi.os = oscChart.addLineSeries({
        color: 'rgba(63,185,80,0.35)', lineWidth: 1, lineStyle: LWC.LineStyle.Dashed,
        priceLineVisible: false, lastValueVisible: false,
    });

    applyActiveVisibility();
}

function setAllSeriesVisible(oscKey, visible) {
    const group = series[oscKey];
    if (!group) return;
    Object.values(group).forEach(s => {
        if (s) try { s.applyOptions({ visible }); } catch (_) { /* noop */ }
    });
}

function applyActiveVisibility() {
    Object.keys(series).forEach(k => setAllSeriesVisible(k, k === _active));
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

function zipHist(times, values) {
    const pts = [];
    for (let i = 0; i < times.length; i++) {
        if (values[i] != null && Number.isFinite(values[i])) {
            pts.push({
                time: times[i], value: values[i],
                color: values[i] >= 0 ? 'rgba(63,185,80,0.8)' : 'rgba(248,81,73,0.8)',
            });
        }
    }
    return pts;
}

function applyOscillators(candles) {
    _candles = Array.isArray(candles) ? candles : [];
    if (_candles.length === 0) return;

    const closes = _candles.map(c => c.close);
    const times = _candles.map(c => c.time);

    const macd = calcMacd(closes);
    series.macd.hist?.setData(zipHist(times, macd.hist));
    series.macd.line?.setData(zip(times, macd.macdLine));
    series.macd.signal?.setData(zip(times, macd.signal));

    const rsi = calcRsi(closes);
    series.rsi.line?.setData(zip(times, rsi));
    series.rsi.ob?.setData(zip(times, times.map(() => 70)));
    series.rsi.os?.setData(zip(times, times.map(() => 30)));

    const adx = calcAdx(_candles);
    series.adx.line?.setData(zip(times, adx));

    const stoch = calcStochRsi(closes);
    series.stochrsi.line?.setData(zip(times, stoch));
    series.stochrsi.ob?.setData(zip(times, times.map(() => 80)));
    series.stochrsi.os?.setData(zip(times, times.map(() => 20)));

    scheduleSnap();
}

function scheduleSnap() {
    requestAnimationFrame(() => requestAnimationFrame(() => {
        const main = getMainChart();
        if (!main || !oscChart) return;
        try {
            const r = main.timeScale().getVisibleLogicalRange();
            if (r) oscChart.timeScale().setVisibleLogicalRange(r);
        } catch (_) { /* noop */ }
    }));
}

function syncTimeScale() {
    const main = getMainChart();
    if (!main || !oscChart) return;
    let syncing = false;
    main.timeScale().subscribeVisibleLogicalRangeChange((range) => {
        if (syncing || !range) return;
        syncing = true;
        oscChart.timeScale().setVisibleLogicalRange(range);
        syncing = false;
    });
}

export function resizeOscPane() {
    const container = document.getElementById('st-chart-osc');
    if (oscChart && container && _visible && container.style.display !== 'none') {
        oscChart.resize(container.clientWidth, container.clientHeight);
    }
}

function applyPaneVisibility() {
    const pane = document.getElementById('st-chart-osc');
    const div = document.getElementById('st-chart-osc-divider');
    const tabs = document.getElementById('st-chart-osc-tabs');
    [pane, div, tabs].forEach(el => { if (el) el.style.display = _visible ? '' : 'none'; });
    if (_visible) requestAnimationFrame(() => resizeOscPane());
}

function initOscToggle() {
    const btn = document.getElementById('st-osc-toggle');
    if (!btn) return;
    btn.classList.toggle('st-tf-btn--active', _visible);
    btn.addEventListener('click', () => {
        _visible = !_visible;
        localStorage.setItem(VIS_KEY, String(_visible));
        btn.classList.toggle('st-tf-btn--active', _visible);
        applyPaneVisibility();
    });
}

function initOscTabs() {
    document.querySelectorAll('.st-osc-tab').forEach(btn => {
        const osc = btn.dataset.osc;
        btn.classList.toggle('st-osc-tab--active', osc === _active);
        btn.addEventListener('click', () => {
            if (osc === _active) return;
            _active = osc;
            localStorage.setItem(ACTIVE_KEY, _active);
            document.querySelectorAll('.st-osc-tab').forEach(b =>
                b.classList.toggle('st-osc-tab--active', b.dataset.osc === _active));
            applyActiveVisibility();
        });
    });
}

function setupDivider(onResize) {
    const div = document.getElementById('st-chart-osc-divider');
    const chartArea = document.getElementById('st-chart');
    const pane = document.getElementById('st-chart-osc');
    if (!div || !chartArea || !pane) return;

    const MIN_PX = 80, MAX_FRAC = 0.40;
    let drag = false, startY = 0, startH = 0;

    const fireResize = () => {
        resizeOscPane();
        // Notify chart.js so main + volume canvases re-measure too.
        emit('chart-pane-resized');
        if (typeof onResize === 'function') onResize();
    };

    div.addEventListener('mousedown', (e) => {
        e.preventDefault();
        drag = true;
        startY = e.clientY;
        startH = pane.getBoundingClientRect().height;
        div.classList.add('st-chart-divider--active');
        document.body.style.cursor = 'row-resize';
        document.body.style.userSelect = 'none';
    });
    document.addEventListener('mousemove', (e) => {
        if (!drag) return;
        const dy = startY - e.clientY;
        const areaH = chartArea.getBoundingClientRect().height;
        const newH = Math.max(MIN_PX, Math.min(areaH * MAX_FRAC, startH + dy));
        pane.style.flex = 'none';
        pane.style.height = newH + 'px';
        fireResize();
    });
    document.addEventListener('mouseup', () => {
        if (!drag) return;
        drag = false;
        div.classList.remove('st-chart-divider--active');
        document.body.style.cursor = '';
        document.body.style.userSelect = '';
        fireResize();
    });
}

function clearOscillators() {
    Object.values(series).forEach((group) => {
        Object.values(group).forEach((s) => {
            if (s) try { s.setData([]); } catch (_) { /* noop */ }
        });
    });
    _candles = [];
}

function setupBusListeners() {
    on('chart-candles', (data) => {
        if (data?.candles) applyOscillators(data.candles);
    });
    on('chart-clear', () => clearOscillators());
}

export function initPanes(resizeFn) {
    createOscPane();
    applyPaneVisibility();
    syncTimeScale();
    setupDivider(resizeFn || (() => resizeOscPane()));
    initOscToggle();
    initOscTabs();
    setupBusListeners();
}

export function disposePanes() {
    try { oscChart?.remove(); } catch (_) { /* noop */ }
    oscChart = null;
    Object.values(series).forEach(g => {
        Object.keys(g).forEach(k => { g[k] = null; });
    });
    _candles = [];
}
