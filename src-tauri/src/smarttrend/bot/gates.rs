use tauri::{AppHandle, Manager};

use crate::market::scanner::AlpacaAsset;
use crate::state::Direction;

use super::SkipReason;

// ═══════════════════════════════════════════════════════════════════════
//  S02 — Gate 0 pure helpers (compliance gates)
// ═══════════════════════════════════════════════════════════════════════
//
// These functions encode the entire compliance-gate decision tree as pure
// total functions over plain values. They are exercised by unit tests that
// don't need TitanState or async runtime.
//
// Wire-up site: `handle_entry` calls each helper before placing an order.

const PDT_EQUITY_THRESHOLD: f64 = 25_000.0;
/// `daytrade_count` value at which the *next* day-trade would be the 4th in
/// the rolling 5-day window (PDT trip). Block at >= 3.
const PDT_DAYTRADE_LIMIT: u32 = 3;
/// Buying-power utilisation cap — leave 2% headroom for fees/slippage.
const BP_SAFETY_FACTOR: f64 = 0.98;
/// Account snapshot is considered stale after 5 minutes (10x equity_sync
/// poll interval of 30s). Beyond this, Gate 0 refuses to evaluate PDT/BP.
const ACCOUNT_STALE_SECS: i64 = 300;

/// Pattern Day Trader rule:
///   - If broker has flagged the account `pattern_day_trader=true` and equity
///     is under $25k, the account is permanently restricted from day trades.
///   - Otherwise: under $25k AND already 3 day trades in the rolling window
///     ⇒ the next entry would be the 4th day trade and trip PDT (90-day
///     restriction).
///
/// S28 (2026-05-28) — Swing-Only Override
///
/// `allow_swing` + `now_et_hour >= swing_hour_start` bypasses the
/// daytrade_count guard when the entry can only become a day trade if the
/// bracket trips inside the remaining RTH window. By default this returns
/// the conservative answer (block). The caller decides whether the entry
/// becomes "swing-intent" (widened bracket, flat-by-close skip).
///
/// The `pattern_day_trader=true` branch is NOT overridable — once the
/// account is broker-flagged PDT, only $25K equity reset can unlock it.
pub(super) fn pdt_gate_check(
    equity: f64,
    daytrade_count: u32,
    pattern_day_trader: bool,
    now_et_hour: u32,
    allow_swing: bool,
    swing_hour_start: u32,
    is_cash_account: bool,
) -> Option<SkipReason> {
    // S40 (audit 2026-05-29): the PDT rule applies ONLY to margin
    // accounts. On a cash account the "3 day-trades / 5 days" limit does
    // not exist — the real constraint is T+1 cash settlement, handled by
    // the buying-power gate (Alpaca shrinks buying_power to settled cash).
    // Verified live: account multiplier=1, pattern_day_trader=false, yet
    // daytrade_count reached 3 and the legacy gate was needlessly
    // blocking every entry on a $300 cash account.
    if is_cash_account {
        return None;
    }
    if pattern_day_trader && equity < PDT_EQUITY_THRESHOLD {
        return Some(SkipReason::PdtBlocked);
    }
    if equity < PDT_EQUITY_THRESHOLD && daytrade_count >= PDT_DAYTRADE_LIMIT {
        // S28: late-RTH swing-intent bypass. We require `now_et_hour < 16`
        // (still inside RTH) — outside RTH the gate is meaningless because
        // entries are rejected by the trading-window gate further upstream.
        if allow_swing && now_et_hour >= swing_hour_start && now_et_hour < 16 {
            return None;
        }
        return Some(SkipReason::PdtBlocked);
    }
    None
}

/// S28 — Public predicate: did the swing-only bypass actually fire?
/// Caller (`handle_entry`) uses this to mark `is_swing_intent` so bracket
/// sizing and `flat_by_close` treat the position correctly. Mirrors the
/// internal branch of `pdt_gate_check` so we don't re-derive the rule in
/// two places.
pub(crate) fn is_swing_intent_entry(
    equity: f64,
    daytrade_count: u32,
    pattern_day_trader: bool,
    now_et_hour: u32,
    allow_swing: bool,
    swing_hour_start: u32,
    is_cash_account: bool,
) -> bool {
    // S40: no PDT on a cash account → the swing-intent bypass (which only
    // exists to dodge the PDT day-trade cap) never applies. Trades stay
    // normal intraday entries.
    if is_cash_account {
        return false;
    }
    if pattern_day_trader && equity < PDT_EQUITY_THRESHOLD {
        return false;
    }
    if equity < PDT_EQUITY_THRESHOLD
        && daytrade_count >= PDT_DAYTRADE_LIMIT
        && allow_swing
        && now_et_hour >= swing_hour_start
        && now_et_hour < 16
    {
        return true;
    }
    false
}

/// Buying-power gate: estimated cost (qty * price + buffer for fees/slippage)
/// must fit in 98% of broker `buying_power`. Note: `est_cost` already
/// represents `qty * price` from caller; the safety factor is applied to
/// `buying_power` here.
pub(super) fn buying_power_gate_check(est_cost: f64, buying_power: f64) -> Option<SkipReason> {
    if !est_cost.is_finite() || !buying_power.is_finite() {
        return Some(SkipReason::BuyingPowerInsufficient);
    }
    if buying_power <= 0.0 {
        return Some(SkipReason::BuyingPowerInsufficient);
    }
    if est_cost > buying_power * BP_SAFETY_FACTOR {
        Some(SkipReason::BuyingPowerInsufficient)
    } else {
        None
    }
}

/// Short selling gate. Returns `None` (pass) immediately for long entries.
/// For short entries, requires:
///   1. `cfg.execution.allow_short == true`
///   2. asset metadata present in cache (cache miss = `AssetUnknown`)
///   3. `shortable && easy_to_borrow == true`
pub(super) fn short_gate_check(
    direction: Direction,
    allow_short: bool,
    asset: Option<&AlpacaAsset>,
) -> Option<SkipReason> {
    if direction != Direction::Short {
        return None;
    }
    if !allow_short {
        return Some(SkipReason::ShortDisabled);
    }
    match asset {
        None => Some(SkipReason::AssetUnknown),
        Some(a) if !a.shortable || !a.easy_to_borrow => Some(SkipReason::NotShortable),
        Some(_) => None,
    }
}

/// S17.2 — Quality Gate decision result. Returned by
/// [`quality_gate_check`] so the caller can render rich blocked-event
/// payloads without having to re-derive the reason here.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum QualityGateOutcome {
    /// Gate is fully disabled in config — allow the entry.
    Disabled,
    /// All requested checks pass — allow the entry.
    Pass,
    /// Symbol is not present in the latest scan; we have no metadata
    /// to evaluate it. Conservative reject.
    NotInScan,
    /// Top-N filter is active and the symbol's rank ≥ N.
    /// `(rank, n)` so callers can log "rank=5, n=3".
    NotInTopN { rank: usize, n: u32 },
    /// Symbol is currently halted per latest scan flags.
    Halted,
    /// Catalyst score is below the configured minimum.
    /// `(score, threshold, pills_csv)` for logging.
    BelowMinScore {
        score: f64,
        threshold: f64,
        pills: String,
    },
}

/// S17.2 — Quality Gate: pure decision over a slice of scan results
/// already sorted by score DESC (which is the contract of
/// `prefilter::run`). Caller is responsible for locking
/// `state.market` and passing the immutable slice in.
///
/// `top_n_filter == 0` disables the top-N check; `catalyst_required
/// == false` disables the catalyst score / halt check. If both are
/// disabled the function short-circuits to `Disabled` without
/// scanning the slice.
/// S20 — Auto-universe fallback. The S19 fresh-born scanner runs
/// every 30min during RTH and typically promotes few candidates
/// per tick. FSM tracking, by contrast, generates signals on the
/// entire `auto_pool` (15+ symbols). The two diverge: high-quality
/// FSM signals were getting rejected because their symbols weren't
/// in the latest scan slice. Resolution: when a symbol is absent
/// from `scan_results` but present in `auto_universe.auto_pool`,
/// accept it using the pool entry's persisted `last_score` as the
/// catalyst proxy. The pool promotion itself already required scan
/// validation (`min_promote_score` / `min_promote_rvol`), so this
/// is a relaxation that respects the original quality contract.
///
/// S20.1 — Pinned-symbol bypass. The original S20 fix only covered
/// `auto_pool`; pinned names still fell through to `NotInScan` when
/// the latest scan slice didn't include them. This was discovered
/// live on 2026-05-20: QBTS (pinned, 1/8 user-managed slots) hit
/// Quality Gate 8 times with `NotInScan` while carrying FSM Entry
/// scores up to 98.0 — every single one of those entries was
/// dropped despite the symbol being explicitly chosen by the
/// operator. Pinned is the *editorial* tier of the universe
/// (user-curated conviction), distinct from `auto_pool` (bot-curated
/// statistical promotion); the Quality Gate's catalyst / top-N
/// filters are statistical guardrails meant for the bot's own
/// promotions. Pinned therefore bypasses both checks: rank-absent
/// pinned names are treated as `Pass`, regardless of whether the
/// scanner found a fresh catalyst score in the most recent tick.
/// The other entry filters (`min_entry_score`, sentinel ATR/
/// efficiency veto, VWAP gate, FSM diagnostic vetoes, entry window,
/// flat-by-close) all remain in force, so this isn't a free pass —
/// it just stops blocking user-trusted symbols on the *scan
/// freshness* axis.
pub(super) fn quality_gate_check(
    scan_results: &[crate::market::scanner::ScanResult],
    auto_universe: &crate::market::auto_universe::AutoUniverse,
    symbol: &str,
    catalyst_required: bool,
    catalyst_min_score: f64,
    top_n_filter: u32,
) -> QualityGateOutcome {
    if !catalyst_required && top_n_filter == 0 {
        return QualityGateOutcome::Disabled;
    }

    let needle = symbol.trim();
    let rank = scan_results
        .iter()
        .position(|r| r.symbol.eq_ignore_ascii_case(needle));

    let pool_entry = auto_universe.auto_pool.get(&needle.to_uppercase());
    let is_pinned = auto_universe
        .pinned
        .iter()
        .any(|s| s.eq_ignore_ascii_case(needle));

    if top_n_filter > 0 {
        match rank {
            Some(idx) if (idx as u32) < top_n_filter => {} // ok
            Some(idx) => {
                return QualityGateOutcome::NotInTopN {
                    rank: idx,
                    n: top_n_filter,
                }
            }
            None if pool_entry.is_some() => {} // S20: tracked → bypass top-N
            None if is_pinned => {}             // S20.1: pinned → bypass top-N
            None => return QualityGateOutcome::NotInScan,
        }
    }

    if catalyst_required {
        match rank.and_then(|i| scan_results.get(i)) {
            Some(entry) => {
                if entry.catalyst_flags.halted {
                    return QualityGateOutcome::Halted;
                }
                if entry.catalyst_flags.score < catalyst_min_score {
                    return QualityGateOutcome::BelowMinScore {
                        score: entry.catalyst_flags.score,
                        threshold: catalyst_min_score,
                        pills: entry.catalyst_flags.pills_owned().join(","),
                    };
                }
            }
            None => match (pool_entry, is_pinned) {
                (Some(e), _) => {
                    if e.last_score < catalyst_min_score {
                        return QualityGateOutcome::BelowMinScore {
                            score: e.last_score,
                            threshold: catalyst_min_score,
                            pills: "auto_pool".to_string(),
                        };
                    }
                }
                // S20.1: user-pinned without a pool/scan footprint —
                // honour the editorial choice, downstream entry
                // filters still apply.
                (None, true) => {}
                (None, false) => return QualityGateOutcome::NotInScan,
            },
        }
    }

    QualityGateOutcome::Pass
}

/// S17.5 — Fresh-momentum / VWAP gate result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum FreshMomentumOutcome {
    /// Filter disabled — allow.
    Disabled,
    /// Above VWAP — allow.
    Pass,
    /// Not enough today-session bars to compute a meaningful VWAP.
    /// Conservative reject (filter is on, data isn't ready).
    InsufficientData,
    /// Live price ≤ session VWAP. `(price, vwap)` for logging.
    BelowVwap { price: f64, vwap: f64 },
}

/// Cumulative session VWAP from a bars slice that is already filtered
/// to today's ET trading session. Returns `None` when the slice is
/// empty or when total volume is zero (illiquid pre-market).
///
/// Pure: no time / TZ logic — caller is responsible for trimming
/// `bars` to today's session before calling.
pub(super) fn session_vwap_from_bars(bars: &[titan_types::Candle]) -> Option<f64> {
    let mut cum_vol = 0.0_f64;
    let mut cum_vp = 0.0_f64;
    for b in bars {
        let typ = (b.high + b.low + b.close) / 3.0;
        if !typ.is_finite() || !b.volume.is_finite() {
            continue;
        }
        cum_vol += b.volume;
        cum_vp += b.volume * typ;
    }
    if cum_vol > 0.0 && cum_vp.is_finite() {
        Some(cum_vp / cum_vol)
    } else {
        None
    }
}

/// Filter `bars` to those whose `time` (UNIX seconds) falls on the same
/// ET calendar date as `now_et_ts`. Used to anchor session VWAP to
/// today's RTH opening rather than the entire ring buffer (which can
/// span multiple sessions).
pub(super) fn bars_in_today_et_session(
    bars: &[titan_types::Candle],
    now_et_ts: i64,
) -> Vec<titan_types::Candle> {
    use chrono::{TimeZone, Datelike};
    let now_et = match chrono_tz::US::Eastern.timestamp_opt(now_et_ts, 0).single() {
        Some(t) => t,
        None => return bars.to_vec(),
    };
    let today = (now_et.year(), now_et.month(), now_et.day());
    bars.iter()
        .filter(|b| {
            match chrono_tz::US::Eastern.timestamp_opt(b.time, 0).single() {
                Some(t) => (t.year(), t.month(), t.day()) == today,
                None => false,
            }
        })
        .cloned()
        .collect()
}

/// Decision: live price relative to today's session VWAP.
pub(super) fn fresh_momentum_check(
    require_above_vwap: bool,
    live_price: f64,
    today_bars: &[titan_types::Candle],
) -> FreshMomentumOutcome {
    if !require_above_vwap {
        return FreshMomentumOutcome::Disabled;
    }
    if today_bars.is_empty() {
        return FreshMomentumOutcome::InsufficientData;
    }
    let vwap = match session_vwap_from_bars(today_bars) {
        Some(v) => v,
        None => return FreshMomentumOutcome::InsufficientData,
    };
    if !live_price.is_finite() || live_price <= 0.0 {
        return FreshMomentumOutcome::InsufficientData;
    }
    if live_price > vwap {
        FreshMomentumOutcome::Pass
    } else {
        FreshMomentumOutcome::BelowVwap {
            price: live_price,
            vwap,
        }
    }
}

/// Account-snapshot freshness gate. Returns `Some(AccountStale)` if the
/// equity_sync loop hasn't successfully written `last_account_sync_at`
/// within `ACCOUNT_STALE_SECS`. `0` = never synced (also stale).
pub(super) fn account_stale_check(now_ts: i64, last_sync_at: i64) -> Option<SkipReason> {
    if last_sync_at <= 0 {
        return Some(SkipReason::AccountStale);
    }
    if now_ts.saturating_sub(last_sync_at) > ACCOUNT_STALE_SECS {
        return Some(SkipReason::AccountStale);
    }
    None
}

/// Helper: emit a `bot-blocked` event with `{symbol, reason}` payload and a
/// matching `tracing::info!` line for log correlation.
///
/// PRIME F1 — also records the veto into the persistent decision
/// journal (stage "compliance") so gate costs are queryable after the
/// tracing logs rotate.
pub(super) fn emit_blocked_event(app: &AppHandle, symbol: &str, reason: &SkipReason) {
    tracing::info!(
        component = "bot",
        symbol,
        reason = %reason,
        "Entry BLOCKED at Gate 0 (compliance)"
    );
    crate::ops::decision_log::log(crate::ops::decision_log::DecisionRecord::vetoed(
        symbol,
        "",
        "bot",
        "none",
        0.0,
        "compliance",
        &reason.to_string(),
        0,
        0.0,
        "unknown",
    ));
    let _ = app.emit_all(
        "bot-blocked",
        serde_json::json!({
            "symbol": symbol,
            "reason": reason.to_string(),
        }),
    );
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use crate::market::scanner::AlpacaAsset;
    use crate::smarttrend::bot_safety::SkipReason;
    use crate::state::Direction;

    use super::{
        account_stale_check, buying_power_gate_check, is_swing_intent_entry, pdt_gate_check,
        short_gate_check,
    };

    // ── S02: PDT gate (legacy semantics, swing-override disabled) ──
    //
    // All legacy tests call with `allow_swing=false`, which makes the
    // new params transparent: behaviour matches the pre-S28 contract.
    const LEGACY_HOUR: u32 = 10; // mid-RTH, irrelevant when allow_swing=false
    const LEGACY_SWING_START: u32 = 15;

    // S40: all legacy/S28 PDT tests are margin-account scenarios →
    // is_cash_account = false (the last arg) preserves prior semantics.
    const MARGIN: bool = false;

    #[test]
    fn pdt_gate_blocks_when_count_3_equity_24k() {
        assert_eq!(
            pdt_gate_check(24_000.0, 3, false, LEGACY_HOUR, false, LEGACY_SWING_START, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
    }

    #[test]
    fn pdt_gate_blocks_when_pattern_day_trader_under_25k() {
        assert_eq!(
            pdt_gate_check(20_000.0, 0, true, LEGACY_HOUR, false, LEGACY_SWING_START, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
    }

    #[test]
    fn pdt_gate_passes_when_equity_above_25k_and_count_3() {
        assert_eq!(
            pdt_gate_check(25_000.0, 3, false, LEGACY_HOUR, false, LEGACY_SWING_START, MARGIN),
            None
        );
        assert_eq!(
            pdt_gate_check(50_000.0, 5, false, LEGACY_HOUR, false, LEGACY_SWING_START, MARGIN),
            None
        );
    }

    #[test]
    fn pdt_gate_passes_when_count_2_under_25k() {
        assert_eq!(
            pdt_gate_check(24_000.0, 2, false, LEGACY_HOUR, false, LEGACY_SWING_START, MARGIN),
            None
        );
    }

    #[test]
    fn pdt_gate_passes_when_pattern_flag_above_25k() {
        assert_eq!(
            pdt_gate_check(30_000.0, 0, true, LEGACY_HOUR, false, LEGACY_SWING_START, MARGIN),
            None
        );
    }

    // ── S40: cash-account bypass (PDT rule does not apply) ──

    #[test]
    fn s40_cash_account_never_pdt_blocked() {
        // Cash account: even count=3 under $25k with broker PDT flag and
        // no swing override must PASS — PDT is a margin-only rule.
        assert_eq!(
            pdt_gate_check(300.0, 3, false, 10, false, 15, true),
            None
        );
        assert_eq!(
            pdt_gate_check(300.0, 9, true, 10, false, 15, true),
            None,
            "cash account bypasses PDT regardless of count / broker flag"
        );
    }

    #[test]
    fn s40_cash_account_no_swing_intent() {
        // No PDT on cash → swing-intent marker is never needed.
        assert!(!is_swing_intent_entry(300.0, 3, false, 15, true, 15, true));
    }

    // ── S28: Swing-only override (PDT bypass in late-RTH window) ──

    #[test]
    fn s28_swing_pass_when_count_3_in_window_with_override_on() {
        // 15:00 ET, PDT count=3, override ON → entry permitted.
        assert_eq!(
            pdt_gate_check(300.0, 3, false, 15, true, 15, MARGIN),
            None,
            "swing override should bypass count=3 inside late-RTH window"
        );
        // 15:30 ET, same → still ok.
        assert_eq!(pdt_gate_check(300.0, 3, false, 15, true, 15, MARGIN), None);
    }

    #[test]
    fn s28_swing_block_before_window_even_with_override_on() {
        // 14:00 ET (before swing window start) → still blocked.
        assert_eq!(
            pdt_gate_check(300.0, 3, false, 14, true, 15, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
        // 10:00 ET → still blocked.
        assert_eq!(
            pdt_gate_check(300.0, 3, false, 10, true, 15, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
    }

    #[test]
    fn s28_swing_block_when_override_off_in_window() {
        // 15:30 ET, override OFF → block (default safety).
        assert_eq!(
            pdt_gate_check(300.0, 3, false, 15, false, 15, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
    }

    #[test]
    fn s28_swing_block_at_or_after_close_hour() {
        // 16:00 ET (close) → block; swing override has no purpose post-RTH.
        assert_eq!(
            pdt_gate_check(300.0, 3, false, 16, true, 15, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
        assert_eq!(
            pdt_gate_check(300.0, 3, false, 18, true, 15, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
    }

    #[test]
    fn s28_swing_does_not_unlock_broker_flagged_pdt() {
        // pattern_day_trader=true broker flag is sticky — cannot be
        // bypassed by the swing window. Even at 15:30 ET with override
        // on, the gate still blocks.
        assert_eq!(
            pdt_gate_check(300.0, 0, true, 15, true, 15, MARGIN),
            Some(SkipReason::PdtBlocked)
        );
    }

    // ── S28: is_swing_intent_entry predicate (downstream marker) ──

    #[test]
    fn s28_predicate_true_inside_window() {
        assert!(is_swing_intent_entry(300.0, 3, false, 15, true, 15, MARGIN));
        assert!(is_swing_intent_entry(300.0, 5, false, 15, true, 15, MARGIN));
    }

    #[test]
    fn s28_predicate_false_outside_window() {
        // Before swing start
        assert!(!is_swing_intent_entry(300.0, 3, false, 14, true, 15, MARGIN));
        // At/after close
        assert!(!is_swing_intent_entry(300.0, 3, false, 16, true, 15, MARGIN));
    }

    #[test]
    fn s28_predicate_false_when_count_under_three() {
        // count=2 → not PDT-blocked, no swing-intent flag necessary.
        assert!(!is_swing_intent_entry(300.0, 2, false, 15, true, 15, MARGIN));
    }

    #[test]
    fn s28_predicate_false_when_override_off() {
        assert!(!is_swing_intent_entry(300.0, 3, false, 15, false, 15, MARGIN));
    }

    #[test]
    fn s28_predicate_false_when_equity_above_threshold() {
        // Account ≥ $25K → PDT rule doesn't apply → no swing-intent.
        assert!(!is_swing_intent_entry(25_000.0, 3, false, 15, true, 15, MARGIN));
    }

    #[test]
    fn s28_predicate_false_when_broker_flag_active() {
        assert!(!is_swing_intent_entry(300.0, 3, true, 15, true, 15, MARGIN));
    }

    // ── S02: Buying-power gate ──

    #[test]
    fn buying_power_gate_blocks_when_est_cost_exceeds() {
        assert_eq!(
            buying_power_gate_check(990.0, 1_000.0),
            Some(SkipReason::BuyingPowerInsufficient)
        );
    }

    #[test]
    fn buying_power_gate_passes_at_98pct_threshold() {
        assert_eq!(buying_power_gate_check(980.0, 1_000.0), None);
    }

    #[test]
    fn buying_power_gate_blocks_on_zero_or_negative() {
        assert_eq!(
            buying_power_gate_check(100.0, 0.0),
            Some(SkipReason::BuyingPowerInsufficient)
        );
        assert_eq!(
            buying_power_gate_check(100.0, -10.0),
            Some(SkipReason::BuyingPowerInsufficient)
        );
    }

    #[test]
    fn buying_power_gate_blocks_on_non_finite() {
        assert_eq!(
            buying_power_gate_check(f64::NAN, 1_000.0),
            Some(SkipReason::BuyingPowerInsufficient)
        );
        assert_eq!(
            buying_power_gate_check(100.0, f64::INFINITY),
            Some(SkipReason::BuyingPowerInsufficient)
        );
    }

    // ── S02: Short gate ──

    fn shortable_asset() -> AlpacaAsset {
        AlpacaAsset {
            symbol: "AAPL".into(),
            tradable: true,
            exchange: "NASDAQ".into(),
            shortable: true,
            easy_to_borrow: true,
            fractionable: true,
            marginable: true,
            maintenance_margin_requirement: Some(30.0),
        }
    }

    #[test]
    fn short_blocked_when_not_shortable() {
        let mut a = shortable_asset();
        a.shortable = false;
        assert_eq!(
            short_gate_check(Direction::Short, true, Some(&a)),
            Some(SkipReason::NotShortable)
        );
    }

    #[test]
    fn short_blocked_when_not_easy_to_borrow() {
        let mut a = shortable_asset();
        a.easy_to_borrow = false;
        assert_eq!(
            short_gate_check(Direction::Short, true, Some(&a)),
            Some(SkipReason::NotShortable)
        );
    }

    #[test]
    fn short_blocked_when_allow_short_disabled() {
        let a = shortable_asset();
        assert_eq!(
            short_gate_check(Direction::Short, false, Some(&a)),
            Some(SkipReason::ShortDisabled)
        );
    }

    #[test]
    fn short_blocked_when_asset_unknown() {
        assert_eq!(
            short_gate_check(Direction::Short, true, None),
            Some(SkipReason::AssetUnknown)
        );
    }

    #[test]
    fn short_passes_when_long_direction() {
        assert_eq!(short_gate_check(Direction::Long, false, None), None);
        let mut a = shortable_asset();
        a.shortable = false;
        assert_eq!(short_gate_check(Direction::Long, true, Some(&a)), None);
    }

    #[test]
    fn short_passes_when_shortable_etb_and_allowed() {
        let a = shortable_asset();
        assert_eq!(short_gate_check(Direction::Short, true, Some(&a)), None);
    }

    // ── S02: Account-stale gate ──

    #[test]
    fn account_stale_when_never_synced() {
        assert_eq!(account_stale_check(1_000_000, 0), Some(SkipReason::AccountStale));
    }

    #[test]
    fn account_stale_when_older_than_5_min() {
        assert_eq!(
            account_stale_check(1_000_000, 1_000_000 - 301),
            Some(SkipReason::AccountStale)
        );
    }

    #[test]
    fn account_stale_passes_when_fresh() {
        assert_eq!(account_stale_check(1_000_000, 1_000_000 - 300), None);
        assert_eq!(account_stale_check(1_000_000, 1_000_000 - 60), None);
    }

    // ── S17.2: Quality Gate ──

    use crate::market::auto_universe::AutoUniverse;
    use crate::market::catalyst::{CatalystFlags, GapDir};
    use crate::market::scanner::ScanResult;
    use super::{quality_gate_check, QualityGateOutcome};

    fn mk_result(symbol: &str, score: f64, halted: bool) -> ScanResult {
        ScanResult {
            symbol: symbol.into(),
            price: 5.0,
            change_percent: 0.0,
            volume: 0.0,
            rvol: 0.0,
            gap_percent: 0.0,
            score: 0.0,
            dollar_volume: 0.0,
            bid: 0.0,
            ask: 0.0,
            spread_bps: 0.0,
            last_trade_age_seconds: 0,
            is_stale: false,
            catalyst_flags: CatalystFlags {
                earnings: false,
                news_recent: score >= 25.0 && !halted,
                halted,
                gap_dir: GapDir::None,
                vol_spike: false,
                offering_risk: false,
                score: if halted { 0.0 } else { score },
            },
            catalyst_pills: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn quality_gate_disabled_when_both_off() {
        // Both filters disabled: empty scan_results is irrelevant.
        let out = quality_gate_check(&[], &AutoUniverse::default(), "FATE", false, 25.0, 0);
        assert_eq!(out, QualityGateOutcome::Disabled);
    }

    #[test]
    fn quality_gate_passes_when_catalyst_score_above_threshold() {
        // Symbol present with catalyst score 50, threshold 25 → pass.
        let scan = vec![mk_result("FATE", 50.0, false)];
        let out = quality_gate_check(&scan, &AutoUniverse::default(), "FATE", true, 25.0, 0);
        assert_eq!(out, QualityGateOutcome::Pass);
    }

    #[test]
    fn quality_gate_blocks_when_catalyst_score_below() {
        // Score 10 < threshold 25 → BelowMinScore.
        let scan = vec![mk_result("DARE", 10.0, false)];
        let out = quality_gate_check(&scan, &AutoUniverse::default(), "DARE", true, 25.0, 0);
        match out {
            QualityGateOutcome::BelowMinScore { score, threshold, .. } => {
                assert!((score - 10.0).abs() < f64::EPSILON);
                assert!((threshold - 25.0).abs() < f64::EPSILON);
            }
            other => panic!("expected BelowMinScore, got {other:?}"),
        }
    }

    #[test]
    fn quality_gate_blocks_halted_even_with_high_score() {
        // Halted dominates: even if score is 100, halt blocks.
        let scan = vec![mk_result("STOP", 100.0, true)];
        let out = quality_gate_check(&scan, &AutoUniverse::default(), "STOP", true, 25.0, 0);
        // mk_result sets score=0 when halted to mirror compute() semantics
        // but `halted=true` still triggers the Halted outcome branch.
        assert_eq!(out, QualityGateOutcome::Halted);
    }

    #[test]
    fn quality_gate_blocks_when_symbol_not_in_scan_and_required() {
        // Symbol absent from scan when catalyst_required=true → reject.
        let scan = vec![mk_result("FATE", 50.0, false)];
        let out = quality_gate_check(&scan, &AutoUniverse::default(), "GHOST", true, 25.0, 0);
        assert_eq!(out, QualityGateOutcome::NotInScan);
    }

    #[test]
    fn quality_gate_top_n_passes_for_top_ranked_symbol() {
        // Top-3: symbol at index 0 must pass.
        let scan = vec![
            mk_result("AAA", 80.0, false),
            mk_result("BBB", 70.0, false),
            mk_result("CCC", 60.0, false),
            mk_result("DDD", 50.0, false),
        ];
        // catalyst disabled, top-3 active
        let out = quality_gate_check(&scan, &AutoUniverse::default(), "AAA", false, 0.0, 3);
        assert_eq!(out, QualityGateOutcome::Pass);
    }

    #[test]
    fn quality_gate_top_n_blocks_for_lower_ranked_symbol() {
        // Top-3: index 3 (= 4th symbol) must be blocked.
        let scan = vec![
            mk_result("AAA", 80.0, false),
            mk_result("BBB", 70.0, false),
            mk_result("CCC", 60.0, false),
            mk_result("DDD", 50.0, false),
        ];
        let out = quality_gate_check(&scan, &AutoUniverse::default(), "DDD", false, 0.0, 3);
        match out {
            QualityGateOutcome::NotInTopN { rank, n } => {
                assert_eq!(rank, 3);
                assert_eq!(n, 3);
            }
            other => panic!("expected NotInTopN, got {other:?}"),
        }
    }

    #[test]
    fn quality_gate_combined_filters_both_apply() {
        // Both gates active: must pass both.
        let scan = vec![
            mk_result("AAA", 80.0, false),
            mk_result("BBB", 70.0, false),
            mk_result("CCC", 10.0, false), // top-3 but low catalyst
        ];
        // CCC: in top-3 but catalyst score 10 < 25
        let out = quality_gate_check(&scan, &AutoUniverse::default(), "CCC", true, 25.0, 3);
        match out {
            QualityGateOutcome::BelowMinScore { score, .. } => {
                assert!((score - 10.0).abs() < f64::EPSILON);
            }
            other => panic!("expected BelowMinScore, got {other:?}"),
        }
    }

    #[test]
    fn quality_gate_top_n_strict_zero_index() {
        // Top-N=1 → only rank 0 passes.
        let scan = vec![
            mk_result("AAA", 80.0, false),
            mk_result("BBB", 70.0, false),
        ];
        assert_eq!(
            quality_gate_check(&scan, &AutoUniverse::default(), "AAA", false, 0.0, 1),
            QualityGateOutcome::Pass
        );
        match quality_gate_check(&scan, &AutoUniverse::default(), "BBB", false, 0.0, 1) {
            QualityGateOutcome::NotInTopN { rank, n } => {
                assert_eq!(rank, 1);
                assert_eq!(n, 1);
            }
            other => panic!("expected NotInTopN, got {other:?}"),
        }
    }

    #[test]
    fn quality_gate_case_insensitive_symbol_match() {
        // Symbol lookup tolerates case mismatch.
        let scan = vec![mk_result("FATE", 50.0, false)];
        assert_eq!(
            quality_gate_check(&scan, &AutoUniverse::default(), "fate", true, 25.0, 0),
            QualityGateOutcome::Pass
        );
        assert_eq!(
            quality_gate_check(&scan, &AutoUniverse::default(), "Fate", true, 25.0, 0),
            QualityGateOutcome::Pass
        );
    }

    // ── S20: Auto-universe fallback ──

    #[test]
    fn quality_gate_passes_when_symbol_in_auto_pool_only() {
        let scan = vec![mk_result("OTHER", 50.0, false)];
        let mut au = AutoUniverse::default();
        au.auto_pool.insert(
            "TRACKED".to_string(),
            crate::market::auto_universe::AutoEntry {
                symbol: "TRACKED".to_string(),
                added_at: chrono::Utc::now(),
                score_at_add: 80.0,
                last_score: 75.0,
                last_seen_in_scan: chrono::Utc::now(),
                low_score_streak: 0,
                realized_pnl: 0.0,
                wins: 0,
                losses: 0,
            },
        );
        let out = quality_gate_check(&scan, &au, "TRACKED", true, 25.0, 0);
        assert_eq!(out, QualityGateOutcome::Pass);
    }

    #[test]
    fn quality_gate_blocks_when_auto_pool_score_below_threshold() {
        let scan = vec![];
        let mut au = AutoUniverse::default();
        au.auto_pool.insert(
            "WEAK".to_string(),
            crate::market::auto_universe::AutoEntry {
                symbol: "WEAK".to_string(),
                added_at: chrono::Utc::now(),
                score_at_add: 60.0,
                last_score: 15.0,
                last_seen_in_scan: chrono::Utc::now(),
                low_score_streak: 0,
                realized_pnl: 0.0,
                wins: 0,
                losses: 0,
            },
        );
        let out = quality_gate_check(&scan, &au, "WEAK", true, 25.0, 0);
        match out {
            QualityGateOutcome::BelowMinScore { score, threshold, .. } => {
                assert!((score - 15.0).abs() < f64::EPSILON);
                assert!((threshold - 25.0).abs() < f64::EPSILON);
            }
            other => panic!("expected BelowMinScore, got {other:?}"),
        }
    }

    // ── S20.1: Pinned-symbol bypass ──

    /// Regression for the QBTS 2026-05-20 incident. Pinned symbol
    /// absent from both `scan_results` and `auto_pool` must still
    /// pass the catalyst-required Quality Gate — pinned is the
    /// editorial user-trust tier and outranks the statistical
    /// catalyst/scan freshness checks.
    #[test]
    fn quality_gate_passes_when_symbol_is_pinned_only() {
        let scan = vec![mk_result("OTHER", 50.0, false)];
        let mut au = AutoUniverse::default();
        au.pinned.push("QBTS".to_string());
        let out = quality_gate_check(&scan, &au, "QBTS", true, 25.0, 0);
        assert_eq!(out, QualityGateOutcome::Pass);
    }

    /// Top-N filter must also yield to pinned: a user-pinned name
    /// outside the top-N slice is still allowed through. Other
    /// downstream filters (min_entry_score, sentinel veto, VWAP)
    /// retain their independent veto power.
    #[test]
    fn quality_gate_pinned_bypasses_top_n_when_not_in_scan() {
        let scan = vec![
            mk_result("AAA", 80.0, false),
            mk_result("BBB", 70.0, false),
            mk_result("CCC", 60.0, false),
        ];
        let mut au = AutoUniverse::default();
        au.pinned.push("QBTS".to_string());
        // top_n=3 active, catalyst off; QBTS absent from scan but pinned.
        let out = quality_gate_check(&scan, &au, "QBTS", false, 0.0, 3);
        assert_eq!(out, QualityGateOutcome::Pass);
    }

    /// Sanity: pinned bypass is case-insensitive, matching the
    /// rest of `quality_gate_check`'s symbol-matching contract.
    #[test]
    fn quality_gate_pinned_bypass_case_insensitive() {
        let scan = vec![mk_result("OTHER", 50.0, false)];
        let mut au = AutoUniverse::default();
        au.pinned.push("qbts".to_string());
        let out = quality_gate_check(&scan, &au, "QBTS", true, 25.0, 0);
        assert_eq!(out, QualityGateOutcome::Pass);
    }

    // ── S17.5: Fresh-momentum / VWAP gate ──

    use super::{fresh_momentum_check, session_vwap_from_bars, FreshMomentumOutcome};

    fn mk_bar(time: i64, h: f64, l: f64, c: f64, v: f64) -> titan_types::Candle {
        titan_types::Candle {
            time,
            open: c,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    #[test]
    fn vwap_returns_none_for_empty() {
        assert_eq!(session_vwap_from_bars(&[]), None);
    }

    #[test]
    fn vwap_returns_none_when_total_volume_zero() {
        let bars = vec![mk_bar(0, 10.0, 9.0, 9.5, 0.0), mk_bar(60, 10.5, 9.5, 10.0, 0.0)];
        assert_eq!(session_vwap_from_bars(&bars), None);
    }

    #[test]
    fn vwap_single_bar_equals_typical_price() {
        // Single bar: VWAP = (high + low + close) / 3
        let bars = vec![mk_bar(0, 10.0, 9.0, 9.5, 1000.0)];
        let v = session_vwap_from_bars(&bars).unwrap();
        let expected = (10.0 + 9.0 + 9.5) / 3.0;
        assert!((v - expected).abs() < 1e-9, "got {v}, expected {expected}");
    }

    #[test]
    fn vwap_volume_weights_correctly() {
        // Bar 1: typ=10, vol=1000 → weighted=10_000
        // Bar 2: typ=20, vol=3000 → weighted=60_000
        // VWAP = 70_000 / 4000 = 17.5
        let bars = vec![
            mk_bar(0, 11.0, 9.0, 10.0, 1000.0),
            mk_bar(60, 21.0, 19.0, 20.0, 3000.0),
        ];
        let v = session_vwap_from_bars(&bars).unwrap();
        assert!((v - 17.5).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn fresh_momentum_disabled_returns_disabled() {
        let bars = vec![mk_bar(0, 10.0, 9.0, 9.5, 1000.0)];
        assert_eq!(
            fresh_momentum_check(false, 5.0, &bars),
            FreshMomentumOutcome::Disabled
        );
    }

    #[test]
    fn fresh_momentum_insufficient_data_when_no_bars() {
        assert_eq!(
            fresh_momentum_check(true, 10.0, &[]),
            FreshMomentumOutcome::InsufficientData
        );
    }

    #[test]
    fn fresh_momentum_pass_when_price_above_vwap() {
        // VWAP ≈ 9.5, price = 11 → pass.
        let bars = vec![mk_bar(0, 10.0, 9.0, 9.5, 1000.0)];
        assert_eq!(
            fresh_momentum_check(true, 11.0, &bars),
            FreshMomentumOutcome::Pass
        );
    }

    #[test]
    fn fresh_momentum_block_when_price_at_or_below_vwap() {
        // VWAP ≈ 9.5
        let bars = vec![mk_bar(0, 10.0, 9.0, 9.5, 1000.0)];
        match fresh_momentum_check(true, 9.5, &bars) {
            FreshMomentumOutcome::BelowVwap { price, vwap } => {
                assert!((price - 9.5).abs() < 1e-9);
                assert!((vwap - 9.5).abs() < 1e-9);
            }
            other => panic!("expected BelowVwap, got {other:?}"),
        }
        // Strictly below
        match fresh_momentum_check(true, 8.0, &bars) {
            FreshMomentumOutcome::BelowVwap { price, vwap: _ } => {
                assert!((price - 8.0).abs() < 1e-9);
            }
            other => panic!("expected BelowVwap, got {other:?}"),
        }
    }

    #[test]
    fn fresh_momentum_blocks_invalid_price() {
        let bars = vec![mk_bar(0, 10.0, 9.0, 9.5, 1000.0)];
        assert_eq!(
            fresh_momentum_check(true, f64::NAN, &bars),
            FreshMomentumOutcome::InsufficientData
        );
        assert_eq!(
            fresh_momentum_check(true, 0.0, &bars),
            FreshMomentumOutcome::InsufficientData
        );
        assert_eq!(
            fresh_momentum_check(true, -5.0, &bars),
            FreshMomentumOutcome::InsufficientData
        );
    }
}
