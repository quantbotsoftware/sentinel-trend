import assert from 'node:assert/strict';
import { test } from 'node:test';
import { formatIntentLine, formatStatusLine, isOperatorMarker, meridianMarkers } from './meridian-markers.js';

const intent = (over = {}) => ({
    symbol: 'SPY', bar_idx: 30, delta_shares: 310, fill_price: 655.2, sleeves: ['A'],
    bar_time: 1788444000, time_et: '10:00', net_after: 310,
    label: 'A · LONG: 30-min close above the upper noise band — trail stop = VWAP / band', ...over,
});

test('markers only for the active symbol, long below bar / exit above bar', () => {
    const payload = { execute: false, new_intents: [intent(), intent({ symbol: 'QQQ' }), intent({ delta_shares: -310, net_after: 0, bar_idx: 60 })] };
    const m = meridianMarkers(payload, 'SPY');
    assert.equal(m.length, 2);
    assert.equal(m[0].shape, 'arrowUp');
    assert.equal(m[0].position, 'belowBar');
    assert.equal(m[0].text, 'MRD·A BUY 310');
    assert.ok(m[0].reason.includes('shadow: no order sent'));
    assert.equal(m[1].shape, 'circle');
    assert.equal(m[1].position, 'aboveBar');
    assert.equal(m[1].text, 'MRD·A SELL 310');
    assert.equal(meridianMarkers(payload, null).length, 0);
    assert.equal(meridianMarkers({ new_intents: [{ symbol: 'SPY' }] }, 'SPY').length, 0, 'malformed intents are dropped');
});

test('short entry is an arrowDown in red', () => {
    const m = meridianMarkers({ execute: true, new_intents: [intent({ delta_shares: -200, net_after: -200 })] }, 'SPY');
    assert.equal(m[0].shape, 'arrowDown');
    assert.equal(m[0].color, '#F85149');
    assert.equal(m[0].display_only, false);
});

test('intent and status lines are human readable', () => {
    assert.equal(formatIntentLine(intent(), false), 'MERIDIAN (shadow) 10:00 · BUY 310 SPY @ 655.20 · A · LONG: 30-min close above the upper noise band — trail stop = VWAP / band');
    assert.equal(formatIntentLine({}, false), '');
    const s = formatStatusLine({ net_shares: { SPY: 310, QQQ: 0 }, equity: 100412.4, bars_today: { SPY: 61, QQQ: 60 } });
    assert.equal(s, 'QQQ 0 · SPY +310 · 61 bars');
    assert.equal(formatStatusLine(null), '—');
});

test('operator marker filter keeps MERIDIAN, real fills and ★ marks; drops legacy FSM/APX/FRT', () => {
    assert.equal(isOperatorMarker({ source: 'meridian', text: 'MRD·A BUY 560' }), true);
    assert.equal(isOperatorMarker({ is_real: true, text: '' }), true);
    assert.equal(isOperatorMarker({ text: '★ BOT BUY' }), true);
    assert.equal(isOperatorMarker({ source: 'fsm', text: 'FSM SELL' }), false);
    assert.equal(isOperatorMarker({ source: 'apex', text: 'APX 0.81 GO' }), false);
    assert.equal(isOperatorMarker({ source: 'frontier', text: 'FRT·TAPE SHADOW' }), false);
    assert.equal(isOperatorMarker(null), false);
});
