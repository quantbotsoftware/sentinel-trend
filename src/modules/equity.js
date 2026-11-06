import { on } from './bus.js';

import { invoke, listen } from './tauri-bridge.js';

let equityChart = null;
let equitySeries = null;
let _resizeObs = null;

// ─── Init ────────────────────────────────────────────────────────────

export async function initEquity() {
    on('tab-journal-active', () => loadEquityCurve());

    listen('bot-pnl', () => {
        const tab = document.getElementById('st-tab-journal');
        if (tab && !tab.classList.contains('st-tab-content--hidden')) {
            loadEquityCurve();
        }
    });
}

// ─── Chart Setup ─────────────────────────────────────────────────────

function ensureChart() {
    if (equityChart) return;

    const container = document.getElementById('st-equity-chart');
    if (!container) return;

    // F1 exception: LWC is loaded via <script> tag, not an ES module.
    const LWC = window.LightweightCharts;
    if (!LWC) return;

    equityChart = LWC.createChart(container, {
        width: container.clientWidth,
        height: 120,
        layout: {
            background: { color: '#161B22' },
            textColor: '#8B949E',
            fontSize: 14,
        },
        grid: {
            vertLines: { color: 'rgba(48,54,61,0.4)' },
            horzLines: { color: 'rgba(48,54,61,0.4)' },
        },
        timeScale: {
            borderColor: '#30363D',
            timeVisible: false,
        },
        rightPriceScale: {
            borderColor: '#30363D',
        },
        crosshair: {
            horzLine: { visible: false },
            vertLine: { visible: false },
        },
    });

    equitySeries = equityChart.addAreaSeries({
        topColor: 'rgba(88,166,255,0.3)',
        bottomColor: 'rgba(88,166,255,0.02)',
        lineColor: '#58A6FF',
        lineWidth: 2,
    });

    _resizeObs = new ResizeObserver(() => {
        if (equityChart && container.clientWidth > 0) {
            equityChart.applyOptions({ width: container.clientWidth });
        }
    });
    _resizeObs.observe(container);
}

// ─── Dispose (called from core.js beforeunload aggregator) ──────────

export function disposeEquity() {
    _resizeObs?.disconnect();
    _resizeObs = null;
    equityChart?.remove();
    equityChart = null;
    equitySeries = null;
}

// ─── Data Loading ────────────────────────────────────────────────────

async function loadEquityCurve() {
    ensureChart();
    if (!equitySeries) return;

    try {
        const points = await invoke('get_equity_curve');
        if (!points || points.length === 0) {
            equitySeries.setData([]);
            return;
        }

        const data = points.map((p) => ({
            time: p.time,
            value: p.value,
        }));

        equitySeries.setData(data);
        equityChart.timeScale().fitContent();
    } catch (err) {
        console.error('Equity curve load failed:', err);
    }
}
