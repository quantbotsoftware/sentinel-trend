// S15.5 Fix #1 — fmtRvol unit tests. Run with `node --test`.
//
// No external test runner; uses Node's built-in `node:test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { fmtRvol, rvolIsCapped } from './fmt.js';

test('fmtRvol below 10 keeps two decimals', () => {
    assert.equal(fmtRvol(0.5), '0.50x');
    assert.equal(fmtRvol(3.21), '3.21x');
});

test('fmtRvol [10, 100) keeps one decimal', () => {
    assert.equal(fmtRvol(56.3), '56.3x');
});

test('fmtRvol [100, 1000) drops decimals', () => {
    assert.equal(fmtRvol(142.7), '143x');
});

test('fmtRvol >= 1000 caps at 999x+', () => {
    assert.equal(fmtRvol(1234), '999x+');
    assert.equal(fmtRvol(211642.9), '999x+');
});

test('fmtRvol NaN / null / undefined / negative all return 0x', () => {
    assert.equal(fmtRvol(NaN), '0x');
    assert.equal(fmtRvol(null), '0x');
    assert.equal(fmtRvol(undefined), '0x');
    assert.equal(fmtRvol(-1), '0x');
});

test('rvolIsCapped trips at 1000 and above only', () => {
    assert.equal(rvolIsCapped(999.9), false);
    assert.equal(rvolIsCapped(1000), true);
    assert.equal(rvolIsCapped(NaN), false);
    assert.equal(rvolIsCapped(null), false);
});
