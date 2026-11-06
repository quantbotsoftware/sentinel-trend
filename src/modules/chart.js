// F1 exception: LightweightCharts is loaded via <script> tag (not an ES module).
// The global `LightweightCharts` reference is unavoidable until an ES module
// build of LWC is adopted (separate task, not S07 scope).
import { on, emit } from './bus.js';
import { fmtNum } from './fmt.js';
import { createVolChart, setupDragDivider } from './volume.js';

import { invoke, listen } from './tauri-bridge.js';

let mainChart = null;
let volChart = null;
let candleSeries = null;
let volSeries = null;
let livePriceLine = null;
let lastBar = null;
let currentSymbol = '';
let currentTF = '5m';
let _loadSeq = 0;
let _volData = [];
let _candles = [];
let _resizeObserver = null;

function volumeBarColor(open, close) {
    return close >= open
        ? 'rgba(88, 166, 255, 0.72)'
        : 'rgba(248, 81, 73, 0.72)';
}

// ─── Chart Initialization ────────────────────────────────────────────

const CHART_OPTS = {
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
    timeScale: {
        borderColor: 'rgba(48, 54, 61, 0.8)',
        timeVisible: true,
        rightOffset: 12,
        shiftVisibleRangeOnNewBar: true,
        // S43 (2026-05-29) — respect the tick-mark TYPE that
        // Lightweight-Charts passes as the 2nd arg. The old formatter
        // always returned HH:MM, so on a multi-day intraday chart
        // (1h/4h/etc.) every day-boundary tick — which lands on the
        // 04:00 ET premarket bar — rendered as "04:00", producing a
        // useless axis of repeated "04:00" labels. Date-level ticks
        // (Year/Month/DayOfMonth) now show the calendar date; only
        // intraday ticks (Time/TimeWithSeconds) show the clock time.
        // Enum values are stable (Year=0, Month=1, DayOfMonth=2,
        // Time=3); numeric fallback keeps it robust if the enum isn't
        // exposed on the global build.
        tickMarkFormatter: (time, tickMarkType) => {
            const d = new Date(time * 1000);
            const TMT = LightweightCharts.TickMarkType || {};
            const isDateTick =
                tickMarkType === TMT.Year ||
                tickMarkType === TMT.Month ||
                tickMarkType === TMT.DayOfMonth ||
                (typeof tickMarkType === 'number' && tickMarkType <= 2);
            if (isDateTick) {
                return d.toLocaleDateString('en-US', {
                    timeZone: 'America/New_York',
                    month: 'short',
                    day: 'numeric',
                });
            }
            return d.toLocaleTimeString('en-US', {
                timeZone: 'America/New_York',
                hour12: false,
                hour: '2-digit',
                minute: '2-digit',
            });
        },
    },
    crosshair: {
        mode: LightweightCharts.CrosshairMode.Normal,
        vertLine: { color: 'rgba(88, 166, 255, 0.4)', width: 1, style: LightweightCharts.LineStyle.Dashed },
        horzLine: { color: 'rgba(88, 166, 255, 0.4)', width: 1, style: LightweightCharts.LineStyle.Dashed },
    },
    rightPriceScale: {
        borderColor: 'rgba(48, 54, 61, 0.8)',
        // Hard-set width matches volume + oscillator panes so candle/
        // histogram bars line up vertically across all three charts.
        // (Volume uses 82, oscillator uses 82 — keep in sync.)
        minimumWidth: 82,
    },
    localization: {
        // S43 — crosshair tooltip shows DATE + time so a hovered bar on
        // a multi-day intraday chart is unambiguous (was HH:MM only,
        // which couldn't tell May-15 04:00 from May-16 04:00).
        timeFormatter: (ts) => {
            const d = new Date(ts * 1000);
            return d.toLocaleString('en-US', {
                timeZone: 'America/New_York',
                month: 'short',
                day: 'numeric',
                hour12: false,
                hour: '2-digit',
                minute: '2-digit',
            });
        },
        dateFormatter: (ts) => {
            const d = new Date(ts * 1000);
            return d.toLocaleDateString('en-US', {
                timeZone: 'America/New_York',
                year: 'numeric',
                month: '2-digit',
                day: '2-digit',
            });
        },
    },
};

function createMainChart(container) {
    mainChart = LightweightCharts.createChart(container, {
        ...CHART_OPTS,
        width: container.clientWidth,
        height: container.clientHeight,
    });

    candleSeries = mainChart.addCandlestickSeries({
        upColor: '#58A6FF',
        downColor: '#F85149',
        borderVisible: false,
        wickUpColor: '#58A6FF',
        wickDownColor: '#F85149',
    });
}

function initVolChart(container) {
    const result = createVolChart(container);
    volChart = result.chart;
    volSeries = result.series;
}

function syncTimeScale() {
    if (!mainChart || !volChart) return;
    let syncing = false;
    mainChart.timeScale().subscribeVisibleLogicalRangeChange((range) => {
        if (syncing || !range) return;
        syncing = true;
        volChart.timeScale().setVisibleLogicalRange(range);
        syncing = false;
    });
}

// ─── Crosshair Sync (BONUS 1) ───────────────────────────────────────

function setupCrosshairSync() {
    if (!mainChart || !volChart) return;

    const mainContainer = document.getElementById('st-chart-main');
    const volContainer = document.getElementById('st-chart-vol');
    if (!mainContainer || !volContainer) return;

    const line = document.createElement('div');
    line.className = 'st-crosshair-line';
    document.getElementById('st-chart').appendChild(line);

    mainChart.subscribeCrosshairMove((param) => {
        if (!param.point || param.point.x < 0) {
            line.style.display = 'none';
            return;
        }
        line.style.display = 'block';
        const chartArea = document.getElementById('st-chart');
        const chartRect = chartArea.getBoundingClientRect();
        const mainRect = mainContainer.getBoundingClientRect();
        line.style.left = `${param.point.x + (mainRect.left - chartRect.left)}px`;
        line.style.height = `${chartRect.height}px`;
        line.style.top = '0';
    });
}

// ─── Data Loading ────────────────────────────────────────────────────

function setLoadingState(loading, errorMsg) {
    const el = document.getElementById('st-chart-loading');
    if (!el) return;
    el.style.display = loading || errorMsg ? 'flex' : 'none';
    const span = el.querySelector('span');
    if (span) span.textContent = errorMsg || 'Loading data...';
}

async function loadHistory(symbol, tf) {
    const mySeq = ++_loadSeq;

    setLoadingState(true);

    // Tell indicator overlays (EMA/VWAP/BB) and oscillator panes to drop any
    // cached series data from the previous symbol BEFORE we apply new candles.
    // Without this, leftover indicator values share the right price-scale and
    // pin auto-scale to the previous symbol's range — new candles then render
    // off-screen and the chart appears blank for non-initial symbols.
    emit('chart-clear', { symbol, tf });

    try {
        const res = await invoke('fetch_alpaca_history', { symbol, timeframe: tf });

        if (mySeq !== _loadSeq) return;

        const data = res.candles;

        if (!data || data.length === 0) {
            setLoadingState(false, `No data for ${symbol} ${tf}`);
            setTimeout(() => { if (mySeq === _loadSeq) setLoadingState(false); }, 4000);
            return;
        }

        data.sort((a, b) => a.time - b.time);

        candleSeries.setData(data);
        _candles = data.slice();

        if (volSeries) {
            _volData = data.map((d) => ({
                time: d.time,
                value: d.volume || 0,
                color: volumeBarColor(d.open, d.close),
            }));
            volSeries.setData(_volData);
        }

        lastBar = { ...data[data.length - 1] };

        updatePriceLine(lastBar.close);
        updateHeaderPrice(lastBar);

        // S34.2 (2026-05-28) — Default visible range per timeframe.
        //
        // Pre-fix: `fitContent()` zoomed to ALL bars returned by
        // `days_for_timeframe` (1h=90d, 4h=180d, 1D=365d). For QBTS
        // that meant 90 days of $13-21 bars rendered, with today's
        // $29 bars cropped by Y autoscale (the bulk of visible bars
        // dominated the scale). Operator saw a price axis (13-21)
        // that didn't match the current price marker (29.54).
        //
        // Fix: cap the visible window to a session-relevant count
        // per TF. Y autoscale then fits the meaningful recent range.
        // Tony can still scroll left manually for historical context.
        applyDefaultVisibleRange(tf, data.length);
        try {
            candleSeries.priceScale().applyOptions({ autoScale: true });
            if (volSeries) {
                volSeries.priceScale().applyOptions({ autoScale: true });
            }
        } catch (_) { /* noop */ }

        setLoadingState(false);
        emit('chart-loaded', { symbol, tf, bars: data.length });
        emit('chart-candles', { symbol, tf, candles: _candles });
    } catch (err) {
        if (mySeq !== _loadSeq) return;
        let raw;
        if (typeof err === 'string') raw = err;
        else if (err?.message) raw = err.message;
        else if (typeof err === 'object') raw = Object.values(err).join(': ');
        else raw = String(err);
        console.error(`[chart] Load failed for ${symbol} ${tf}:`, raw);
        setLoadingState(false, raw);
    }
}

// S34.2 (2026-05-28) — Per-timeframe default visible bar count.
//
// Each value targets "the last few RTH sessions worth of bars" so
// the price scale defaults to a meaningful recent range. Operator
// can scroll left for historical context (data still loaded).
//
// One RTH session = 6.5h = 390 minutes:
//   1m  → ~390 bars, 5m  → 78, 10m → 39, 15m → 26,
//   30m → 13, 1h → 6.5, 4h → 1.6, 1D → 1.
//
// Defaults below show roughly 1-3 sessions on intraday and ~6-12
// weeks on hourly/daily — wide enough for trend context, tight
// enough to keep Y autoscale anchored to current price.
// S35.3 (2026-05-28 14:55 ET) — Per-TF default visible bars tightened.
//
// Tony reported on round-3 screenshots that 30m / 1h / 15m / 4h charts
// were showing the right-side price scale "out of range" with the
// current price label way above the visible candles. Root cause: the
// visible window covered too many days of historical low-price action
// (QBTS dipped to $13-15 two weeks ago vs. today's $29). The price
// axis autoscale averages over visible bars, so the bulk of older
// low-price bars dominated and crowded today's catalyst pump bars
// into the top sliver.
//
// Fix: cap the default window to ~1-2 sessions on intraday, ~2-3
// weeks on hourly/daily so the visible price range is anchored to
// recent action. Tony can scroll left manually for historical
// context (data still loaded, just not in the initial view).
//
// One RTH session = 6.5h = 390 minutes.
// S36.1 (2026-05-28 19:16 ET) — Tony explicitly asked to MAXIMIZE
// the default visible window so historical FSM markers (e.g. last
// QBTS:1m markers from May 27 12:47 ET, ~30h ago) are visible
// without manual scroll. Each TF's default window is now sized to
// match its `days_for_timeframe` backend fetch range. Trade-off:
// the price-axis autoscale will average over a wider price range,
// re-introducing some "gap" feel on volatile assets, but Tony
// prioritized visibility over tight scaling.
//
// Per-TF math (RTH ≈ 6.5h/day; 1d uses ~5/7 calendar→trading ratio):
//   1m   = 5 days  × 6.5h × 60 = 1950 RTH bars       → 1500 (gentle)
//   5m   = 10 days × 6.5h × 12 = 780 RTH bars        → 750
//   10m  = 10 days × 6.5h × 6  = 390 RTH bars        → 400
//   15m  = 30 days × 6.5h × 4  = 780 RTH bars        → 750
//   30m  = 30 days × 6.5h × 2  = 390 RTH bars        → 400
//   1h   = 90 days × 6.5h      = 585 RTH bars        → 600
//   4h   = 180 days × 6.5h / 4 = 292 RTH bars        → 300
//   1D   = 365 days × 5/7      = 260 trading bars    → 260
const TF_DEFAULT_VISIBLE_BARS = {
    '1m': 1500,   // ~5 RTH days (matches 5-day backend fetch)
    '5m': 750,    // ~10 RTH days
    '10m': 400,   // ~10 RTH days
    '15m': 750,   // ~30 calendar days
    '30m': 400,   // ~30 calendar days
    '1h': 600,    // ~90 calendar days (~3 months)
    '4h': 300,    // ~6 months
    '1d': 260,    // ~1 year of trading days
    '1D': 260,
};

function applyDefaultVisibleRange(tf, totalBars) {
    if (!mainChart || totalBars <= 0) return;
    const wanted = TF_DEFAULT_VISIBLE_BARS[tf] || 100;
    const showLast = Math.min(totalBars, wanted);
    if (showLast >= totalBars) {
        // Less data than requested — fitContent() is fine.
        try { mainChart.timeScale().fitContent(); } catch (_) { /* noop */ }
        return;
    }
    try {
        mainChart.timeScale().setVisibleLogicalRange({
            from: totalBars - showLast,
            to: totalBars,
        });
    } catch (_) {
        // Fallback to fitContent if the logical range API rejects.
        try { mainChart.timeScale().fitContent(); } catch (_) { /* noop */ }
    }
}

// S35.4 (2026-05-28 15:40 ET) — overlays.js calls this after markers
// load to ensure the latest FSM Entry marker is visible. If the
// marker is to the LEFT of the current visible window, expand the
// window leftward to include it (with a 10% buffer beyond the
// marker time so it doesn't sit exactly on the edge). This is
// purely additive — never narrows the user's chosen zoom.
export function ensureMarkerVisible(latestMarkerTimeSec) {
    if (!mainChart || !latestMarkerTimeSec) return;
    try {
        const ts = mainChart.timeScale();
        const visible = ts.getVisibleRange();
        if (!visible || !visible.from) return;
        const leftmost = Number(visible.from);
        const rightmost = Number(visible.to);
        if (latestMarkerTimeSec >= leftmost) return; // already visible
        // Expand left edge to include marker minus 10% of current width.
        const width = rightmost - leftmost;
        const newFrom = latestMarkerTimeSec - Math.max(width * 0.1, 3600);
        ts.setVisibleRange({ from: newFrom, to: rightmost });
    } catch (_) { /* noop */ }
}

function updatePriceLine(price) {
    if (!candleSeries) return;
    if (livePriceLine) {
        livePriceLine.applyOptions({ price });
    } else {
        livePriceLine = candleSeries.createPriceLine({
            price,
            color: '#58A6FF',
            lineWidth: 1,
            lineStyle: LightweightCharts.LineStyle.Dotted,
            // The candle series already labels the last price on the axis;
            // a second label here would duplicate it.
            axisLabelVisible: false,
            title: '',
        });
    }
}

// ─── Symbol Header (BONUS 2) ────────────────────────────────────────

function updateHeaderPrice(bar) {
    const priceEl = document.getElementById('st-header-price');
    const changeEl = document.getElementById('st-header-change');
    if (!priceEl || !bar) return;

    priceEl.textContent = `$${fmtNum(bar.close)}`;

    if (changeEl && bar.open > 0) {
        const changePctVal = (bar.close - bar.open) / bar.open * 100;
        const changePct = fmtNum(changePctVal);
        const sign = changePctVal >= 0 ? '+' : '';
        changeEl.textContent = `${sign}${changePct}%`;
        changeEl.className = 'st-header-change';
        changeEl.classList.add(changePct >= 0 ? 'st-header-change--up' : 'st-header-change--down');
    }
}

// ─── Live Updates ────────────────────────────────────────────────────

function tfToSecs(tf) {
    const map = { '1m': 60, '5m': 300, '10m': 600, '15m': 900, '30m': 1800, '1h': 3600, '4h': 14400, '1D': 86400, '1d': 86400 };
    return map[tf] || 60;
}

function setupLiveListeners() {
    listen('price-update', (event) => {
        const { symbol, price, size, ts } = event.payload;
        if (symbol !== currentSymbol) return;

        const priceEl = document.getElementById('st-header-price');
        if (priceEl) priceEl.textContent = `$${fmtNum(price)}`;

        updatePriceLine(price);

        if (!lastBar || !candleSeries) return;

        const secs = tfToSecs(currentTF);
        const tradeTs = ts || Math.floor(Date.now() / 1000);
        const bucket = Math.floor(tradeTs / secs) * secs;

        const vol = size || 0;

        if (bucket > lastBar.time) {
            lastBar = { time: bucket, open: price, high: price, low: price, close: price, volume: vol };
        } else if (bucket === lastBar.time) {
            lastBar.close = price;
            lastBar.high = Math.max(lastBar.high, price);
            lastBar.low = Math.min(lastBar.low, price);
            lastBar.volume = (lastBar.volume || 0) + vol;
        } else {
            return;
        }

        candleSeries.update(lastBar);
        updateHeaderPrice(lastBar);

        if (volSeries && vol > 0) {
            const volPoint = {
                time: lastBar.time,
                value: lastBar.volume || 0,
                color: volumeBarColor(lastBar.open, lastBar.close),
            };
            volSeries.update(volPoint);
        }
    });

    document.addEventListener('visibilitychange', () => {
        if (!document.hidden && currentSymbol && candleSeries) {
            loadHistory(currentSymbol, currentTF);
        }
    });

    listen('bar-closed', (event) => {
        const { symbol, timeframe, open, high, low, close, volume, time } = event.payload;
        if (symbol !== currentSymbol) return;

        const tfMatch =
            timeframe === currentTF ||
            (currentTF === '1D' && timeframe === '1d');

        if (!tfMatch) return;

        lastBar = { time, open, high, low, close, volume };
        candleSeries.update(lastBar);

        // Maintain _candles buffer for indicator subscribers
        if (_candles.length > 0 && _candles[_candles.length - 1].time === time) {
            _candles[_candles.length - 1] = { ...lastBar };
        } else {
            _candles.push({ ...lastBar });
            const MAX = 2000;
            if (_candles.length > MAX) _candles.splice(0, _candles.length - MAX);
        }

        if (volSeries) {
            const volPoint = {
                time,
                value: volume || 0,
                color: volumeBarColor(open, close),
            };
            _volData.push(volPoint);
            volSeries.update(volPoint);
        }

        updatePriceLine(close);
        emit('chart-candles', { symbol: currentSymbol, tf: currentTF, candles: _candles });
    });
}

// ─── Timeframe Switch ────────────────────────────────────────────────

function setupTFButtons() {
    document.querySelectorAll('.st-tf-btn').forEach((btn) => {
        btn.addEventListener('click', () => {
            const tf = btn.dataset.tf;
            if (tf && tf !== currentTF) {
                setTimeframe(tf);
            }
        });
    });
}

function setTimeframe(tf) {
    applyTimeframe(tf, true);
}

function applyTimeframe(tf, emitEvent) {
    if (!tf || tf === currentTF) return;
    currentTF = tf;

    document.querySelectorAll('.st-tf-btn').forEach((btn) => {
        btn.classList.toggle('st-tf-btn--active', btn.dataset.tf === tf);
    });

    if (emitEvent) emit('tf-changed', { tf });

    if (currentSymbol) {
        loadHistory(currentSymbol, tf);
    }
}

// ─── Resize ──────────────────────────────────────────────────────────

function resizeCharts() {
    const mainContainer = document.getElementById('st-chart-main');
    const volContainer = document.getElementById('st-chart-vol');

    if (mainChart && mainContainer) {
        mainChart.resize(mainContainer.clientWidth, mainContainer.clientHeight);
    }
    if (volChart && volContainer) {
        volChart.resize(volContainer.clientWidth, volContainer.clientHeight);
    }
}

function setupResize() {
    const chartArea = document.getElementById('st-chart');
    if (!chartArea) return;

    _resizeObserver = new ResizeObserver(() => resizeCharts());
    _resizeObserver.observe(chartArea);

    // Drag on the oscillator divider (handled in indicators-panes.js) mutates
    // pane heights out-of-band, so its callback pings us through the bus to
    // refresh the main + volume canvas dimensions.
    on('chart-pane-resized', () => resizeCharts());
}

// ─── Dispose (called from core.js beforeunload aggregator) ──────────

export function disposeChart() {
    _resizeObserver?.disconnect();
    _resizeObserver = null;
    mainChart?.remove();
    mainChart = null;
    volChart?.remove();
    volChart = null;
    candleSeries = null;
    volSeries = null;
    livePriceLine = null;
}

// ─── Public Accessors (for overlays.js — F3 compliant via init) ─────

export function getCandleSeries() {
    return candleSeries;
}

export function getMainChart() {
    return mainChart;
}

// ─── Public Init ─────────────────────────────────────────────────────

export async function initChart() {
    const mainContainer = document.getElementById('st-chart-main');
    const volContainer = document.getElementById('st-chart-vol');

    if (!mainContainer) {
        console.error('[chart] #st-chart-main not found');
        return;
    }

    // LWC bakes clientWidth/clientHeight into the canvas at creation time.
    // If the flex layout hasn't settled yet the canvas is 0x0 -> empty chart.
    await waitForLayout(mainContainer);

    createMainChart(mainContainer);
    if (volContainer) initVolChart(volContainer);
    syncTimeScale();
    setupCrosshairSync();
    setupDragDivider(resizeCharts);
    setupResize();
    setupTFButtons();
    setupLiveListeners();

    resizeCharts();

    try {
        const sym = await invoke('get_active_symbol');
        currentSymbol = sym && sym.length > 0 ? sym : 'AAPL';
    } catch {
        currentSymbol = 'AAPL';
    }

    const symbolEl = document.getElementById('st-header-symbol');
    if (symbolEl) symbolEl.textContent = currentSymbol;

    const defaultTF = currentTF;
    document.querySelectorAll('.st-tf-btn').forEach((btn) => {
        btn.classList.toggle('st-tf-btn--active', btn.dataset.tf === defaultTF);
    });

    await loadHistory(currentSymbol, currentTF);

    try {
        const cfg = await invoke('get_config');
        const showVol = cfg?.ui?.chart_show_volume ?? true;
        const volContainer = document.getElementById('st-chart-vol');
        if (volContainer) volContainer.style.display = showVol ? '' : 'none';
    } catch { /* default: visible */ }
}

function waitForLayout(el) {
    return new Promise((resolve) => {
        if (el.clientWidth > 0 && el.clientHeight > 0) return resolve();
        requestAnimationFrame(() => requestAnimationFrame(resolve));
    });
}

/// Y pixel of the last close on the main pane (for the bar countdown
/// badge, which docks under the price label); null when unavailable.
export function lastPriceCoordinate() {
    if (!lastBar || !candleSeries) return null;
    const y = candleSeries.priceToCoordinate(lastBar.close);
    return Number.isFinite(y) ? y : null;
}

on('symbol-changed', async (data) => {
    const sym = typeof data === 'string' ? data : data?.symbol;
    if (typeof sym !== 'string' || sym.length === 0 || sym === currentSymbol) return;

    currentSymbol = sym;
    const symbolEl = document.getElementById('st-header-symbol');
    if (symbolEl) symbolEl.textContent = currentSymbol;

    livePriceLine = null;
    lastBar = null;

    await loadHistory(currentSymbol, currentTF);
});

on('tf-changed', (data) => {
    const tf = data?.tf;
    if (typeof tf === 'string' && tf.length > 0) {
        applyTimeframe(tf, false);
    }
});

on('config-saved', async () => {
    try {
        const cfg = await invoke('get_config');
        const volContainer = document.getElementById('st-chart-vol');
        if (volContainer) {
            volContainer.style.display = cfg?.ui?.chart_show_volume !== false ? '' : 'none';
            resizeCharts();
        }
    } catch { /* noop */ }
});
