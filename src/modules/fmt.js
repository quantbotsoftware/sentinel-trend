export function fmtNum(v, d = 2) {
    return Number.isFinite(v) ? v.toFixed(d) : '—';
}

export function fmtPct(v, d = 2) {
    if (!Number.isFinite(v)) return '—';
    const sign = v >= 0 ? '+' : '';
    return `${sign}${v.toFixed(d)}%`;
}

export function fmtMoney(v) {
    if (!Number.isFinite(v)) return '—';
    if (Math.abs(v) >= 1_000_000) return `$${(v / 1_000_000).toFixed(2)}M`;
    if (Math.abs(v) >= 1_000) return `$${(v / 1_000).toFixed(1)}K`;
    return `$${v.toFixed(2)}`;
}

export function fmtPnl(v) {
    if (!Number.isFinite(v)) return '—';
    const sign = v >= 0 ? '+' : '';
    return `${sign}$${v.toFixed(2)}`;
}

// S15.5 — Relative volume display with visual cap.
//
// Penny stocks with prev_volume < 1k can produce RVOL values in the
// tens of thousands (mathematically correct, visually useless). Cap
// the rendered value while keeping the actual number reachable via
// `rvolIsCapped(v)` for tooltip hints.
export function fmtRvol(val) {
    if (val === null || val === undefined || !Number.isFinite(val)) return '0x';
    const v = Number(val);
    if (v < 0) return '0x';
    if (v >= 1000) return '999x+';
    if (v >= 100) return `${v.toFixed(0)}x`;
    if (v >= 10) return `${v.toFixed(1)}x`;
    return `${v.toFixed(2)}x`;
}

export function rvolIsCapped(val) {
    return Number.isFinite(val) && Number(val) >= 1000;
}
