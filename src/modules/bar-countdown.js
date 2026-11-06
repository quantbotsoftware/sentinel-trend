// Bar-close countdown badge on the main chart pane.
//
// Shows the time remaining until the current candle of the active
// timeframe closes (e.g. "0:42" on 1m, "3:17" on 5m). Intraday TFs
// align to epoch multiples (same convention the bar aggregator uses);
// 1D counts down to the 16:00 ET session close.
//
// Lifecycle per S07: the 1s interval is captured in _intervals and
// cleared by disposeBarCountdown().

import { on } from './bus.js';
import { lastPriceCoordinate } from './chart.js';

const TF_SECS = {
    '1m': 60,
    '5m': 300,
    '10m': 600,
    '15m': 900,
    '30m': 1800,
    '1h': 3600,
    '4h': 14400,
};

let _tf = '5m';
let _el = null;
const _intervals = [];

function pad(n) {
    return String(n).padStart(2, '0');
}

function fmtRemaining(secs) {
    if (secs >= 3600) {
        const h = Math.floor(secs / 3600);
        return `${h}:${pad(Math.floor((secs % 3600) / 60))}:${pad(secs % 60)}`;
    }
    return `${Math.floor(secs / 60)}:${pad(secs % 60)}`;
}

/// Seconds until the current bar closes; null when unknown TF.
export function secondsToBarClose(tf, nowMs = Date.now()) {
    const step = TF_SECS[tf];
    const nowSec = Math.floor(nowMs / 1000);
    if (step) {
        return step - (nowSec % step);
    }
    if (tf === '1D' || tf === '1d') {
        // Session close 16:00 ET, DST-safe via Intl.
        const parts = new Intl.DateTimeFormat('en-US', {
            timeZone: 'America/New_York',
            hour12: false,
            hour: 'numeric',
            minute: 'numeric',
            second: 'numeric',
        }).formatToParts(new Date(nowMs));
        const get = (t) => Number(parts.find((p) => p.type === t)?.value || 0);
        const nowEt = (get('hour') % 24) * 3600 + get('minute') * 60 + get('second');
        const close = 16 * 3600;
        return nowEt < close ? close - nowEt : null; // after close: hide
    }
    return null;
}

// Dock the badge on the price scale, right under the last-price label
// (TradingView-style). Falls back to bottom-right when no data yet.
function reposition() {
    const y = lastPriceCoordinate();
    const host = _el.parentElement;
    if (y != null && host) {
        const top = Math.min(Math.max(y + 11, 0), host.clientHeight - 26);
        _el.style.top = `${Math.round(top)}px`;
        _el.style.bottom = 'auto';
    } else {
        _el.style.top = 'auto';
        _el.style.bottom = '8px';
    }
}

function tick() {
    if (!_el) return;
    const secs = secondsToBarClose(_tf);
    if (secs == null) {
        _el.style.display = 'none';
        return;
    }
    _el.style.display = 'block';
    _el.textContent = fmtRemaining(secs);
    reposition();
    // Last 10 seconds of the bar: gold — the FSM decides on bar close.
    _el.classList.toggle('st-bar-countdown--closing', secs <= 10);
}

export function initBarCountdown() {
    // The badge is declared statically in index.html so it exists (and is
    // visible as "--:--") even if a later init step stalls; we only update it.
    _el = document.getElementById('st-bar-countdown');
    if (!_el) return;

    const active = document.querySelector('.st-tf-btn--active');
    if (active?.dataset.tf) _tf = active.dataset.tf;
    on('tf-changed', (data) => {
        if (data?.tf) _tf = data.tf;
        tick();
    });

    _intervals.push(setInterval(tick, 1000));
    tick();
}

export function disposeBarCountdown() {
    _intervals.forEach(clearInterval);
    _intervals.length = 0;
    _el = null;
}
