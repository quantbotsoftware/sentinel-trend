// S15.5 Fix #3 — derivePillsFromFlags unit tests. `node --test`.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
    derivePillsFromFlags,
    pillClassFor,
    renderCatalystPills,
} from './catalyst-pills.js';

test('halt short-circuits all other flags (mirrors Rust pills())', () => {
    const flags = {
        halted: true,
        earnings: true,
        news_recent: true,
        gap_dir: 'Up',
        vol_spike: true,
    };
    assert.deepEqual(derivePillsFromFlags(flags), ['HALT']);
});

test('combinations render in canonical order EARN/NEWS/GAP\u2191/VOL!', () => {
    const flags = {
        halted: false,
        earnings: true,
        news_recent: true,
        gap_dir: 'Up',
        vol_spike: true,
    };
    assert.deepEqual(
        derivePillsFromFlags(flags),
        ['EARN', 'NEWS', 'GAP\u2191', 'VOL!'],
    );
});

test('GapDir Down renders GAP\u2193 (and excludes GAP\u2191)', () => {
    const flags = { halted: false, gap_dir: 'Down', vol_spike: true };
    assert.deepEqual(derivePillsFromFlags(flags), ['GAP\u2193', 'VOL!']);
});

test('GapDir None / missing produces no GAP pill', () => {
    assert.deepEqual(
        derivePillsFromFlags({ halted: false, gap_dir: 'None' }),
        [],
    );
    assert.deepEqual(
        derivePillsFromFlags({ halted: false }),
        [],
    );
});

test('null / undefined / missing fields safely return []', () => {
    assert.deepEqual(derivePillsFromFlags(null), []);
    assert.deepEqual(derivePillsFromFlags(undefined), []);
    assert.deepEqual(derivePillsFromFlags({}), []);
});

test('renderCatalystPills wraps every label through escapeHtml', () => {
    const html = renderCatalystPills(['EARN', 'GAP\u2191']);
    assert.match(html, /<span class="st-pill st-pill--earn">EARN<\/span>/);
    assert.match(html, /<span class="st-pill st-pill--gap-up">GAP\u2191<\/span>/);
});

test('renderCatalystPills empty / non-array returns empty string', () => {
    assert.equal(renderCatalystPills([]), '');
    assert.equal(renderCatalystPills(null), '');
    assert.equal(renderCatalystPills(undefined), '');
});

test('pillClassFor unknown label falls back to base class', () => {
    assert.equal(pillClassFor('UNKNOWN'), 'st-pill');
    assert.equal(pillClassFor('HALT'), 'st-pill st-pill--halt');
});
