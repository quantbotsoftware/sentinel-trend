// Volume pane: chart creation, formatting, drag separator.
// Split from chart.js for F2 compliance (max 400 LOC per module).

// F1 exception: LWC loaded via <script> tag; ES module build is a separate task.
const LWC = window.LightweightCharts;
const VOLUME_NUMBER_FORMAT = new Intl.NumberFormat('en-US', {
    maximumFractionDigits: 0,
});

export function formatVolume(val) {
    return VOLUME_NUMBER_FORMAT.format(Math.round(Number(val) || 0));
}

const VOL_SCALE_MARGINS = { top: 0.05, bottom: 0 };

const VOL_CHART_OPTS = {
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
        scaleMargins: VOL_SCALE_MARGINS,
        minimumWidth: 82,
    },
    crosshair: {
        mode: LWC.CrosshairMode.Normal,
        vertLine: { color: 'rgba(88, 166, 255, 0.4)', width: 1, style: LWC.LineStyle.Dashed },
        horzLine: { color: 'rgba(88, 166, 255, 0.4)', width: 1, style: LWC.LineStyle.Dashed },
    },
};

export function createVolChart(container) {
    const chart = LWC.createChart(container, {
        ...VOL_CHART_OPTS,
        width: container.clientWidth,
        height: container.clientHeight,
    });

    const series = chart.addHistogramSeries({
        // Keep the badge/line exact because the last printed volume matters operationally.
        priceFormat: { type: 'custom', formatter: formatVolume },
        priceScaleId: 'right',
        priceLineVisible: true,
        lastValueVisible: true,
        base: 0,
        color: 'rgba(88, 166, 255, 0.72)',
    });
    series.priceScale().applyOptions({
        scaleMargins: VOL_SCALE_MARGINS,
    });

    return { chart, series };
}

export function setupDragDivider(resizeChartsFn) {
    const divider = document.getElementById('st-chart-divider');
    const chartArea = document.getElementById('st-chart');
    const volPane = document.getElementById('st-chart-vol');

    if (!divider || !chartArea || !volPane) return;

    const MIN_VOL_PX = 60;
    const MAX_VOL_FRAC = 0.40;

    let dragging = false;
    let startY = 0;
    let startVolHeight = 0;

    divider.addEventListener('mousedown', (e) => {
        e.preventDefault();
        dragging = true;
        startY = e.clientY;
        startVolHeight = volPane.getBoundingClientRect().height;
        divider.classList.add('st-chart-divider--active');
        document.body.style.cursor = 'row-resize';
        document.body.style.userSelect = 'none';
    });

    document.addEventListener('mousemove', (e) => {
        if (!dragging) return;
        const dy = startY - e.clientY;
        const areaH = chartArea.getBoundingClientRect().height;
        const maxVolPx = areaH * MAX_VOL_FRAC;
        const newVolH = Math.max(MIN_VOL_PX, Math.min(maxVolPx, startVolHeight + dy));
        volPane.style.flex = 'none';
        volPane.style.height = newVolH + 'px';
        resizeChartsFn();
    });

    document.addEventListener('mouseup', () => {
        if (!dragging) return;
        dragging = false;
        divider.classList.remove('st-chart-divider--active');
        document.body.style.cursor = '';
        document.body.style.userSelect = '';
        resizeChartsFn();
    });
}
