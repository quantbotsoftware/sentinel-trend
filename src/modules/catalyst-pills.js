// S15.5 Fix #3 — pills rendering single source of truth.
//
// The UI ALWAYS derives pills client-side from `catalyst_flags`. The
// server-emitted `catalyst_pills` field is retained on the wire for
// backward compatibility but the UI ignores it — by construction this
// makes pill/column drift (BRLS-style: GAP↑ pill on, GAP% column 0.0%)
// impossible.
//
// Mirrors the Rust contract in `CatalystFlags::pills()` exactly:
//   - HALT short-circuits all other pills (except OFFER — dilution
//     risk outlives the halt)
//   - order: OFFER, EARN, NEWS, GAP↑/↓, VOL!
//
// SC9 invariant: every pill label flows through `escapeHtml` inside
// `renderCatalystPills` even though our flag→label mapping is fixed.
import { escapeHtml, escapeAttr } from './esc.js';

export function derivePillsFromFlags(flags) {
    if (!flags) return [];
    if (flags.halted) return flags.offering_risk ? ['HALT', 'OFFER'] : ['HALT'];
    const out = [];
    if (flags.offering_risk) out.push('OFFER');
    if (flags.earnings) out.push('EARN');
    if (flags.news_recent) out.push('NEWS');
    if (flags.gap_dir === 'Up') out.push('GAP\u2191');
    else if (flags.gap_dir === 'Down') out.push('GAP\u2193');
    if (flags.vol_spike) out.push('VOL!');
    return out;
}

/**
 * Map a catalyst pill label to its CSS class. Unknown labels still
 * render (defensive) but use the base `.st-pill` only.
 */
export function pillClassFor(label) {
    switch (label) {
        case 'HALT':  return 'st-pill st-pill--halt';
        case 'OFFER': return 'st-pill st-pill--offer';
        case 'EARN':  return 'st-pill st-pill--earn';
        case 'NEWS':  return 'st-pill st-pill--news';
        case 'GAP\u2191': return 'st-pill st-pill--gap-up';
        case 'GAP\u2193': return 'st-pill st-pill--gap-down';
        case 'VOL!':  return 'st-pill st-pill--vol';
        default:      return 'st-pill';
    }
}

/**
 * F1 / SC9: each pill label runs through escapeHtml.
 * NEWS pills are clickable when `symbol` is provided — opens catalyst
 * news modal (headline + article URL).
 *
 * @param {string[]} pills
 * @param {{ symbol?: string }} [opts]
 */
export function renderCatalystPills(pills, opts = {}) {
    if (!Array.isArray(pills) || pills.length === 0) return '';
    const symbol = String(opts.symbol || '').trim().toUpperCase();
    return pills.map((p) => {
        const label = String(p);
        const cls = pillClassFor(label);
        if (label === 'NEWS' && symbol) {
            return `<button type="button" class="${cls} st-pill--clickable" data-news-symbol="${escapeAttr(symbol)}" title="Open catalyst news">${escapeHtml(label)}</button>`;
        }
        return `<span class="${cls}">${escapeHtml(label)}</span>`;
    }).join('');
}
