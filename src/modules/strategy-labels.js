// Shared strategy id → human-readable label map.
//
// Single source of truth for strategy display names across the UI
// (Strategy Lab chips/tables + Tournament leaderboard). The order and
// ids mirror the Rust `StrategyId::ALL` enum in
// `src-tauri/src/smarttrend/strategies/mod.rs`.
//
// F3 — shared service module (no cross-module stateful imports).

export const STRATEGIES = [
    ['gap_fade', 'Gap Fade'],
    ['etf_pair', 'ETF Pair'],
    ['afternoon_fade', 'Afternoon Fade'],
    ['stat_arb_50', 'Stat Arb 50'],
    ['leveraged_etf', 'Leveraged ETF'],
    ['catalyst_frontrun', 'Catalyst Front-Run'],
    ['halt_resume', 'Halt Resume'],
    ['extended_news', 'Extended News'],
    ['opening_range_breakout', 'Opening Range Breakout'],
];

const LABELS = new Map(STRATEGIES);

// Returns the human-readable label for a strategy id, falling back to
// the raw id when unknown (forward-compat with new backend strategies).
export function strategyLabel(id) {
    return LABELS.get(id) || id;
}
