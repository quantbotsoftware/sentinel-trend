// Pure-JS math for oscillator indicators: MACD, RSI (Wilder), ADX (Wilder), StochRSI.
// Split out of indicators-panes.js so panes stays under R2 LOC cap and math is unit-testable.

import { calcEma } from './indicators.js';

export function calcMacd(closes) {
    const ema12 = calcEma(closes, 12);
    const ema26 = calcEma(closes, 26);
    const macdLine = new Array(closes.length);
    for (let i = 0; i < closes.length; i++) {
        macdLine[i] = ema12[i] - ema26[i];
    }
    const signal = calcEma(macdLine, 9);
    const hist = new Array(closes.length);
    for (let i = 0; i < closes.length; i++) {
        hist[i] = macdLine[i] - signal[i];
    }
    return { macdLine, signal, hist };
}

export function calcRsi(closes, period = 14) {
    const n = closes.length;
    const out = new Array(n).fill(null);
    if (n < period + 1) return out;

    let avgGain = 0, avgLoss = 0;
    for (let i = 1; i <= period; i++) {
        const d = closes[i] - closes[i - 1];
        if (d > 0) avgGain += d;
        else avgLoss += -d;
    }
    avgGain /= period;
    avgLoss /= period;

    out[period] = avgLoss === 0 ? 100 : 100 - 100 / (1 + avgGain / avgLoss);
    for (let i = period + 1; i < n; i++) {
        const d = closes[i] - closes[i - 1];
        const gain = d > 0 ? d : 0;
        const loss = d < 0 ? -d : 0;
        avgGain = (avgGain * (period - 1) + gain) / period;
        avgLoss = (avgLoss * (period - 1) + loss) / period;
        out[i] = avgLoss === 0 ? 100 : 100 - 100 / (1 + avgGain / avgLoss);
    }
    return out;
}

export function calcAdx(candles, period = 14) {
    const n = candles.length;
    const adx = new Array(n).fill(null);
    if (n < period * 2) return adx;

    const tr = new Array(n).fill(0);
    const pdm = new Array(n).fill(0);
    const ndm = new Array(n).fill(0);

    for (let i = 1; i < n; i++) {
        const c = candles[i];
        const p = candles[i - 1];
        const up = c.high - p.high;
        const dn = p.low - c.low;
        pdm[i] = up > dn && up > 0 ? up : 0;
        ndm[i] = dn > up && dn > 0 ? dn : 0;
        tr[i] = Math.max(
            c.high - c.low,
            Math.abs(c.high - p.close),
            Math.abs(c.low - p.close),
        );
    }

    let trSum = 0, pdmSum = 0, ndmSum = 0;
    for (let i = 1; i <= period; i++) {
        trSum += tr[i];
        pdmSum += pdm[i];
        ndmSum += ndm[i];
    }

    const dx = new Array(n).fill(null);
    if (trSum > 0) {
        const pdi = 100 * pdmSum / trSum;
        const ndi = 100 * ndmSum / trSum;
        const sum = pdi + ndi;
        dx[period] = sum === 0 ? 0 : 100 * Math.abs(pdi - ndi) / sum;
    }

    for (let i = period + 1; i < n; i++) {
        trSum = trSum - trSum / period + tr[i];
        pdmSum = pdmSum - pdmSum / period + pdm[i];
        ndmSum = ndmSum - ndmSum / period + ndm[i];
        if (trSum <= 0) continue;
        const pdi = 100 * pdmSum / trSum;
        const ndi = 100 * ndmSum / trSum;
        const sum = pdi + ndi;
        dx[i] = sum === 0 ? 0 : 100 * Math.abs(pdi - ndi) / sum;
    }

    let adxSum = 0;
    for (let i = period; i < period * 2; i++) adxSum += dx[i] || 0;
    adx[period * 2 - 1] = adxSum / period;

    for (let i = period * 2; i < n; i++) {
        const prev = adx[i - 1];
        const curDx = dx[i] == null ? 0 : dx[i];
        adx[i] = (prev * (period - 1) + curDx) / period;
    }
    return adx;
}

export function calcStochRsi(closes, rsiPeriod = 14, stochPeriod = 14) {
    const rsi = calcRsi(closes, rsiPeriod);
    const n = rsi.length;
    const out = new Array(n).fill(null);

    for (let i = stochPeriod - 1; i < n; i++) {
        let lo = Infinity, hi = -Infinity, count = 0;
        for (let j = i - stochPeriod + 1; j <= i; j++) {
            const v = rsi[j];
            if (v == null) continue;
            count++;
            if (v < lo) lo = v;
            if (v > hi) hi = v;
        }
        if (count < stochPeriod / 2) continue;
        if (hi === lo) { out[i] = 50; continue; }
        out[i] = 100 * (rsi[i] - lo) / (hi - lo);
    }
    return out;
}
