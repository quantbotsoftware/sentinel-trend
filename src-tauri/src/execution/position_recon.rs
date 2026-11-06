use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Deserialize;
use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::ops::journal::{record_trade_close, record_trade_open, TradeOpenParams};
use crate::state::{Direction, FsmPhase, Position, TitanState};
use rust_decimal::prelude::FromPrimitive;

const RECON_INTERVAL_SECS: u64 = 15;

/// S16.3 — captured outside the trading lock so that journal writes (which
/// acquire `journal(6)`) happen after `trading(4)` is dropped, satisfying
/// the global lock ordering rule.
///
/// S16.5 — extended with `invested` + `source` so that the same hint can
/// drive the wallet credit when `trade_updates` misses a fill event.
/// `record_trade_close` is idempotent (it only flips the entry once), so
/// gating the credit on its return value prevents double-credit if a late
/// fill ever arrives.
struct JournalCloseHint {
    trade_id: String,
    symbol: String,
    exit_price: f64,
    pnl: f64,
    invested: f64,
    source: String,
    /// S17.3 — broker's avg_entry at time of close detection. Used to
    /// recompute realized P/L from actual fills (instead of stale
    /// `unrealized_pnl` snapshot) once `/v2/account/activities/FILL`
    /// is queried after the trading-lock is released.
    avg_entry: f64,
    /// S17.3 — broker's qty at time of close detection. Pairs with
    /// `avg_entry` for the realized-P/L recomputation.
    qty: f64,
    /// P0-B (audit 2026-05-29) — bracket levels + side + open time
    /// captured from the local `Position` so the close path can classify
    /// the real `exit_reason` (stop_loss / take_profit / time_stop)
    /// instead of the uniform "BrokerSync". `0.0` stop/target means the
    /// position had no bracket trail (legacy / externally opened).
    stop_loss: f64,
    take_profit: f64,
    opened_at: Option<i64>,
}

struct JournalOpenHint {
    trade_id: String,
    symbol: String,
    side: Direction,
    entry_price: f64,
    qty: f64,
    entry_time: i64,
    /// v2.5.1 HF — Faza 5 attribution for recon-discovered opens.
    ///
    /// When `trade_updates` WebSocket drops fill events (observed
    /// premarket 2026-05-18: bot submitted ~150 orders with sources
    /// like `s2:halt_resume` / `s2:catalyst_frontrun` but none
    /// produced local `Position` rows because the WS never delivered
    /// the fills), `position_recon` is the path that materialises
    /// the position. The original code hardcoded `strategy_id: None`
    /// at the journal call, which severed the per-strategy P/L
    /// attribution required by Faza 5 — the TOURNAMENT tab showed
    /// zero P/L on a session where every fill traced back to a
    /// Tournament order in `trading.order_history`.
    ///
    /// This field carries the recovered strategy identifier (parsed
    /// via `dplus::parse_strategy_from_source` from the most recent
    /// matching order's `source`). `None` means no matching order
    /// was found in history — genuine externally-opened position.
    strategy_id: Option<String>,
    /// Same recovery for `Position.source` — the canonical
    /// `s2:<strategy>` prefix consumed by D-PLUS hooks. Empty string
    /// preserves prior behaviour for externally-opened positions.
    source: String,
    /// Recovered strategy-side entry score from the matching
    /// `order_history` row. `Some` for Tournament strategy entries
    /// (whose `OrderRequest.entry_score` was set at submission);
    /// `None` for legacy momentum / externally-opened positions, in
    /// which case the journaller falls back to the live FSM score.
    /// This is the fix for the all-zero `entry_score` on every
    /// recon-materialised strategy trade (the common paper path).
    entry_score: Option<f64>,
    /// Recovered timeframe (`OrderRequest.entry_timeframe`) from the
    /// matching `order_history` row, so the journal records which TF the
    /// entry fired on even on the silent paper fill path. Empty when no
    /// matching order carried a TF (legacy / externally-opened).
    entry_timeframe: String,
}

/// P0-B (audit 2026-05-29) — classify the real exit reason at close.
///
/// Pre-audit, every recon-detected close was journalled as the uniform
/// `"BrokerSync"`, which made 100% of 1066 journal rows uninformative —
/// we could not tell whether stops, targets, or time-stops were firing,
/// the exact data needed to fix the 26.5% win rate. With the bracket
/// levels now propagated onto `Position` (and into `JournalCloseHint`),
/// this classifies the close:
///   * `stop_loss` / `take_profit` — exit landed on a bracket leg
///     (within a 1% band of the nearer level).
///   * `time_stop` — no bracket leg hit and the position was held
///     ≥ 55 min (max-hold / EOD flat territory).
///   * `signal_exit_win` / `signal_exit_loss` — honest fallback when the
///     mechanism is unknown (legacy / no-bracket / mid-range close); the
///     P/L sign is still captured for analysis.
fn classify_exit_reason(
    exit_price: f64,
    stop_loss: f64,
    take_profit: f64,
    opened_at: Option<i64>,
    pnl: f64,
) -> &'static str {
    if stop_loss > 0.0 && take_profit > 0.0 && exit_price > 0.0 {
        let d_stop = (exit_price - stop_loss).abs();
        let d_target = (exit_price - take_profit).abs();
        let band = exit_price * 0.01;
        if d_stop.min(d_target) <= band {
            return if d_stop <= d_target {
                "stop_loss"
            } else {
                "take_profit"
            };
        }
    }
    if let Some(opened) = opened_at {
        let held_secs = chrono::Utc::now().timestamp().saturating_sub(opened);
        if held_secs >= 55 * 60 {
            return "time_stop";
        }
    }
    if pnl >= 0.0 {
        "signal_exit_win"
    } else {
        "signal_exit_loss"
    }
}

/// v2.5.1 HF — recover Faza 5 attribution for a broker-only position.
///
/// **Context** (live evidence 2026-05-18 09:30 ET): the
/// `trade_updates` WebSocket can silently miss fill events. When that
/// happens, premarket orders submitted with sources like
/// `s2:halt_resume` execute at the open but never produce a local
/// `Position` row — `position_recon`'s broker-only branch then creates
/// a synthetic Position with `source: ""` and `strategy_id: None`,
/// severing the per-strategy P/L attribution required by Faza 5.
///
/// **Recovery strategy**: scan `order_history` newest-first for an
/// order that matches the position's symbol AND entry side (buy for
/// Long, sell for Short), is in a non-terminal-failure state, and
/// carries a non-empty `source`. Return the canonical
/// `(source, strategy_id)` pair.
///
/// **Why strict side match**: a Long position must NOT pick up a
/// close-side `sell` order. Reusing this same recovery for an
/// already-closed position would attribute the new (re-opened) row
/// to the exit order's source, corrupting Tournament P/L. We
/// therefore require entry-side direction agreement.
///
/// **Why filter `failed`/`canceled`/`rejected`**: these orders
/// produced no fill, so attributing a fresh broker-side position to
/// them is wrong — the fill came from a *different* (likely later)
/// order that succeeded. By skipping terminal-failure rows we naturally
/// fall through to the last successful submit for the same side, which
/// is the order that produced the open position.
///
/// Returns `(String::new(), None)` when no eligible order is found —
/// preserving prior behaviour for genuinely externally-opened
/// positions (e.g. discretionary trade entered via Alpaca dashboard).
fn recover_attribution_from_order_history(
    order_history: &[crate::state::OrderHistoryEntry],
    symbol: &str,
    side: Direction,
) -> (String, Option<String>) {
    let expected_order_side = match side {
        Direction::Long => "buy",
        Direction::Short => "sell",
    };
    let recovered_source = order_history
        .iter()
        .rev()
        .find(|o| {
            o.symbol.eq_ignore_ascii_case(symbol)
                && o.side.eq_ignore_ascii_case(expected_order_side)
                && !o.source.is_empty()
                && o.status.as_str() != "failed"
                && o.status.as_str() != "canceled"
                && o.status.as_str() != "rejected"
        })
        .map(|o| o.source.clone())
        .unwrap_or_default();
    let recovered_strategy_id = if recovered_source.is_empty() {
        None
    } else {
        crate::smarttrend::dplus::parse_strategy_from_source(&recovered_source)
            .map(|sid| sid.as_str().to_string())
    };
    if !recovered_source.is_empty() {
        tracing::info!(
            component = "position_recon",
            symbol = %symbol,
            side = ?side,
            source = %recovered_source,
            strategy_id = ?recovered_strategy_id,
            "Recovered Faza 5 attribution for broker-only position from order_history"
        );
    }
    (recovered_source, recovered_strategy_id)
}

/// Recover the strategy-side `entry_score` for a broker-only position
/// from `order_history`, using the same strict side/status matching as
/// [`recover_attribution_from_order_history`]. Returns the score of the
/// most recent successful entry-side order for `symbol`, or `None` when
/// no eligible order carried a score (legacy momentum / external open).
fn recover_entry_score_from_order_history(
    order_history: &[crate::state::OrderHistoryEntry],
    symbol: &str,
    side: Direction,
) -> Option<f64> {
    let expected_order_side = match side {
        Direction::Long => "buy",
        Direction::Short => "sell",
    };
    order_history
        .iter()
        .rev()
        .find(|o| {
            o.symbol.eq_ignore_ascii_case(symbol)
                && o.side.eq_ignore_ascii_case(expected_order_side)
                && o.status.as_str() != "failed"
                && o.status.as_str() != "canceled"
                && o.status.as_str() != "rejected"
        })
        .and_then(|o| o.entry_score)
}

/// Recover the `entry_timeframe` for a broker-only position from
/// `order_history`, using the same strict side/status matching as
/// [`recover_entry_score_from_order_history`]. Returns the TF of the most
/// recent successful entry-side order for `symbol`, or empty when no
/// eligible order carried one (legacy momentum / external open).
fn recover_entry_timeframe_from_order_history(
    order_history: &[crate::state::OrderHistoryEntry],
    symbol: &str,
    side: Direction,
) -> String {
    let expected_order_side = match side {
        Direction::Long => "buy",
        Direction::Short => "sell",
    };
    order_history
        .iter()
        .rev()
        .find(|o| {
            o.symbol.eq_ignore_ascii_case(symbol)
                && o.side.eq_ignore_ascii_case(expected_order_side)
                && o.status.as_str() != "failed"
                && o.status.as_str() != "canceled"
                && o.status.as_str() != "rejected"
                && !o.entry_timeframe.is_empty()
        })
        .map(|o| o.entry_timeframe.clone())
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════════
//  Alpaca position response shape
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
struct AlpacaPosition {
    symbol: String,
    qty: String,
    side: String,
    avg_entry_price: String,
    current_price: Option<String>,
    unrealized_pl: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════
//  Fetch positions from Alpaca REST API
// ═══════════════════════════════════════════════════════════════════════

async fn fetch_positions(state: &TitanState) -> Option<Vec<AlpacaPosition>> {
    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };

    if key.is_empty() || secret.is_empty() {
        return None;
    }

    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    let url = format!("{}/v2/positions", base_url);

    match state
        .http_client
        .get(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                tracing::warn!(
                    component = "position_recon",
                    status = %resp.status(),
                    "Failed to fetch positions"
                );
                return None;
            }
            match resp.json::<Vec<AlpacaPosition>>().await {
                Ok(positions) => Some(positions),
                Err(e) => {
                    tracing::warn!(component = "position_recon", "JSON parse error: {e}");
                    None
                }
            }
        }
        Err(e) => {
            tracing::warn!(component = "position_recon", "HTTP error: {e}");
            None
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  S17.3 — Realized-P/L recomputation from /v2/account/activities/FILL
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
struct AlpacaFillActivity {
    symbol: String,
    side: String,
    qty: String,
    price: String,
    /// Reserved for future use (sort-by-time when partial fills land
    /// out of order). Currently only volume-weighted-mean is needed.
    #[allow(dead_code)]
    transaction_time: String,
}

/// Fetches today's FILL activities for `symbol` and returns the
/// volume-weighted average exit price (across SELL fills) plus the
/// total quantity sold today. Returns `None` when the fetch fails or
/// the symbol has no SELL fills today (multi-day position closed
/// today won't trigger this path because position_recon only flags
/// closes when broker stops returning the position — meaning today's
/// fills DO include the closing SELL).
///
/// The function is intentionally "today-only": multi-leg, multi-day
/// trades are still computed correctly because broker's
/// `avg_entry_price` (cost basis) is honoured by the caller as the
/// entry-side anchor and only the EXIT side needs fill aggregation.
async fn fetch_today_realized_exit(
    state: &TitanState,
    symbol: &str,
) -> Option<(f64, f64)> {
    let (key, secret, paper) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
            cfg.market.paper_mode,
        )
    };
    if key.is_empty() || secret.is_empty() {
        return None;
    }
    let base_url = if paper {
        "https://paper-api.alpaca.markets"
    } else {
        "https://api.alpaca.markets"
    };

    // ET-business-day filter: covers PM/AH activity that still bears
    // today's date in Eastern time, which is how Alpaca buckets fills.
    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let date_et = now_et.format("%Y-%m-%d").to_string();
    let url = format!("{base_url}/v2/account/activities/FILL?date={date_et}");

    let resp = state
        .http_client
        .get(&url)
        .header("APCA-API-KEY-ID", &key)
        .header("APCA-API-SECRET-KEY", &secret)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::debug!(
            component = "position_recon",
            status = %resp.status(),
            symbol,
            "fetch_today_realized_exit: non-success status"
        );
        return None;
    }

    let fills: Vec<AlpacaFillActivity> = resp.json().await.ok()?;

    let needle = symbol.trim().to_uppercase();
    let mut total_qty = 0.0_f64;
    let mut weighted_price_sum = 0.0_f64;
    for f in &fills {
        if !f.symbol.eq_ignore_ascii_case(&needle) {
            continue;
        }
        if !f.side.eq_ignore_ascii_case("sell") {
            continue;
        }
        let q = match f.qty.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => v,
            _ => continue,
        };
        let p = match f.price.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => v,
            _ => continue,
        };
        weighted_price_sum += p * q;
        total_qty += q;
    }

    if total_qty <= 0.0 {
        return None;
    }
    let avg_exit_price = weighted_price_sum / total_qty;
    Some((avg_exit_price, total_qty))
}

/// Pure FIFO P/L: `(avg_exit - avg_entry) * qty`. Multi-fill exits at
/// different prices are folded into `avg_exit` by the caller, so this
/// function is total and trivially testable.
pub(crate) fn realized_pnl_simple(avg_entry: f64, avg_exit: f64, qty: f64) -> f64 {
    if !avg_entry.is_finite() || !avg_exit.is_finite() || !qty.is_finite() {
        return 0.0;
    }
    (avg_exit - avg_entry) * qty
}

// ═══════════════════════════════════════════════════════════════════════
//  Reconciliation loop
// ═══════════════════════════════════════════════════════════════════════

/// Periodically fetches positions from Alpaca and reconciles with local state.
/// Runs every 15 seconds. Updates current_price, unrealized_pnl, and
/// detects new/closed positions.
pub async fn run_position_recon(state: Arc<TitanState>, app: AppHandle) {
    tracing::info!(component = "position_recon", "Position reconciliation loop started");

    let mut ticker = tokio::time::interval(Duration::from_secs(RECON_INTERVAL_SECS));
    let mut last_count: usize = 0;

    loop {
        ticker.tick().await;

        if state.shutdown.is_cancelled() {
            break;
        }

        {
            let mut sys = state.system.lock().unwrap();
            if let Some(h) = sys.loops.get_mut("position_recon") {
                h.tick();
            }
        }

        // S05: snapshot position versions BEFORE the HTTP round-trip so we
        // can detect trade_updates writes that arrived during the fetch.
        let pre_versions: HashMap<String, u64> = {
            let trading = state.trading.lock().unwrap();
            trading.positions.iter().map(|p| (p.symbol.clone(), p.version)).collect()
        };

        let broker_positions = match fetch_positions(&state).await {
            Some(p) => p,
            None => continue,
        };

        // Build broker snapshot keyed by symbol
        let mut broker_map: HashMap<String, (Decimal, Decimal, f64, f64, Direction)> = HashMap::new();
        for bp in &broker_positions {
            let qty = Decimal::from_str(&bp.qty).unwrap_or(Decimal::ZERO).abs();
            if qty <= Decimal::ZERO {
                continue;
            }
            let entry_price = Decimal::from_str(&bp.avg_entry_price).unwrap_or(Decimal::ZERO);
            let current_price = bp
                .current_price
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or_else(|| bp.avg_entry_price.parse::<f64>().unwrap_or(0.0));
            let unrealized_pnl = bp
                .unrealized_pl
                .as_deref()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let is_short = bp.side.eq_ignore_ascii_case("short");
            let side = if is_short { Direction::Short } else { Direction::Long };
            broker_map.insert(bp.symbol.to_uppercase(), (qty, entry_price, current_price, unrealized_pnl, side));
        }

        // S05 CAS-merge: apply broker data without stomping on concurrent
        // trade_updates writes.
        // S16.3: Track positions that disappeared / appeared between recon
        // ticks so we can backfill the journal even when trade_updates
        // websocket misses fills (it has been observed dropping events
        // silently, leaving the journal empty for entire sessions).
        let local_positions: Vec<Position>;
        let mut journal_closes: Vec<JournalCloseHint> = Vec::new();
        let mut journal_opens: Vec<JournalOpenHint> = Vec::new();
        {
            let mut trading = state.trading.lock().unwrap();
            let now_ts = chrono::Utc::now().timestamp();

            let mut merged: Vec<Position> = Vec::new();

            // 1) Update or keep existing positions
            for existing in &trading.positions {
                if let Some(&(bqty, bentry, bprice, bpnl, bside)) = broker_map.get(&existing.symbol) {
                    let pre_ver = pre_versions.get(&existing.symbol).copied().unwrap_or(0);
                    if existing.version != pre_ver {
                        // trade_updates mutated qty/avg_entry during fetch —
                        // only refresh market-data fields from broker
                        let mut pos = existing.clone();
                        pos.current_price = bprice;
                        pos.unrealized_pnl = bpnl;
                        merged.push(pos);
                    } else {
                        // No concurrent write — full broker overwrite
                        merged.push(Position {
                            symbol: existing.symbol.clone(),
                            qty: bqty,
                            avg_entry: bentry,
                            current_price: bprice,
                            unrealized_pnl: bpnl,
                            side: bside,
                            opened_at: existing.opened_at.or(Some(existing.first_seen)),
                            source: existing.source.clone(),
                            first_seen: existing.first_seen,
                            version: existing.version,
                            trade_id: existing.trade_id.clone(),
                            is_swing_intent: existing.is_swing_intent,
                            entry_timeframe: existing.entry_timeframe.clone(),
                            stop_loss: existing.stop_loss,
                            take_profit: existing.take_profit,
                        });
                    }
                    broker_map.remove(&existing.symbol);
                } else {
                    // Broker no longer has this symbol — remove only if
                    // trade_updates didn't touch it during the fetch
                    let pre_ver = pre_versions.get(&existing.symbol).copied().unwrap_or(0);
                    if existing.version != pre_ver {
                        merged.push(existing.clone());
                    } else {
                        // S16.3: Genuinely closed at broker. Record an estimated
                        // exit in the journal using the last known unrealized
                        // P/L as realized P/L (best available estimate without
                        // querying broker order history).
                        // S16.5: Capture `invested` + `source` so the wallet
                        // credit can be applied when trade_updates websocket
                        // misses the fill event (observed in production:
                        // websocket connects + auths but never delivers
                        // fills, leaving the bot wallet permanently debited).
                        let invested_f = (existing.avg_entry * existing.qty)
                            .to_f64()
                            .unwrap_or(0.0);
                        let avg_entry_f = existing.avg_entry.to_f64().unwrap_or(0.0);
                        let qty_f = existing.qty.to_f64().unwrap_or(0.0);
                        journal_closes.push(JournalCloseHint {
                            trade_id: existing.trade_id.clone(),
                            symbol: existing.symbol.clone(),
                            exit_price: existing.current_price,
                            pnl: existing.unrealized_pnl,
                            invested: invested_f,
                            source: existing.source.clone(),
                            avg_entry: avg_entry_f,
                            qty: qty_f,
                            stop_loss: existing.stop_loss,
                            take_profit: existing.take_profit,
                            opened_at: existing.opened_at,
                        });
                    }
                }
            }

            // 2) New positions only seen at broker (not locally)
            for (sym, (bqty, bentry, bprice, bpnl, bside)) in &broker_map {
                let qty_f = bqty.to_f64().unwrap_or(0.0);
                let entry_f = bentry.to_f64().unwrap_or(0.0);
                let synthetic_id = format!("recon-{sym}-{now_ts}");

                // v2.5.1 HF — Faza 5 attribution recovery. See
                // `recover_attribution_from_order_history` for the
                // strict side/status filtering rationale and the live
                // 2026-05-18 incident that motivated this path.
                let (recovered_source, recovered_strategy_id) =
                    recover_attribution_from_order_history(
                        &trading.order_history,
                        sym,
                        *bside,
                    );
                let recovered_entry_score = recover_entry_score_from_order_history(
                    &trading.order_history,
                    sym,
                    *bside,
                );
                let recovered_entry_timeframe = recover_entry_timeframe_from_order_history(
                    &trading.order_history,
                    sym,
                    *bside,
                );

                merged.push(Position {
                    symbol: sym.clone(),
                    qty: *bqty,
                    avg_entry: *bentry,
                    current_price: *bprice,
                    unrealized_pnl: *bpnl,
                    side: *bside,
                    opened_at: Some(now_ts),
                    source: recovered_source.clone(),
                    first_seen: now_ts,
                    version: 0,
                    trade_id: synthetic_id.clone(),
                    is_swing_intent: false,
                    // S37: use recovered TF when order_history carries it;
                    // empty only for genuine externally-opened positions
                    // (SignalExit cross-TF gate bypassed via empty sentinel).
                    entry_timeframe: recovered_entry_timeframe.clone(),
                    // P0-B: broker-recovered position — no bracket trail
                    // available, so exit_reason classification falls back
                    // to the hold-time heuristic.
                    stop_loss: 0.0,
                    take_profit: 0.0,
                });
                journal_opens.push(JournalOpenHint {
                    trade_id: synthetic_id,
                    symbol: sym.clone(),
                    side: *bside,
                    entry_price: entry_f,
                    qty: qty_f,
                    entry_time: now_ts,
                    strategy_id: recovered_strategy_id,
                    source: recovered_source,
                    entry_score: recovered_entry_score,
                    entry_timeframe: recovered_entry_timeframe,
                });
            }

            trading.positions = merged;
            // S16.6 — release pending-exit gates for any symbol that's
            // no longer in trading.positions (broker confirmed close).
            // Without this, a stuck pending_exits entry would block
            // re-entry on a brand-new signal until the 30s timeout.
            let live_syms: std::collections::HashSet<String> =
                trading.positions.iter().map(|p| p.symbol.clone()).collect();
            trading
                .pending_exits
                .retain(|sym, _| live_syms.contains(sym));
            local_positions = trading.positions.clone();
        }

        // S16.3: Drain hints into the journal AFTER releasing trading(4).
        // record_trade_open / record_trade_close acquire journal(6) which is
        // strictly higher in the lock order, so no inversion can occur here.
        //
        // v2.4.1 — pull the latest FSM score for each symbol before
        // draining so the journal row captures the strategy-side
        // signal that triggered the trade. Before this fix all 153
        // entries in the 2026-05-11 journal showed `entry_score=0.0`
        // because the websocket trade-update stream silently delivers
        // no fill events on paper, so every open routes through this
        // recon path which historically hardcoded zero. Reading the
        // FSM via a single smarttrend(3) lock — strictly below
        // journal(6) per the global lock order — has no inversion
        // risk. If the FSM dropped the symbol since order placement
        // we fall back to 0.0 (same legacy behaviour, but now the
        // score-bearing common path no longer loses data).
        let fsm_scores: std::collections::HashMap<String, (f64, FsmPhase)> = if journal_opens.is_empty() {
            std::collections::HashMap::new()
        } else {
            let st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
            journal_opens
                .iter()
                .map(|h| {
                    let (score, phase) = st
                        .fsm
                        .get(&h.symbol)
                        .map(|f| (f.score, f.phase))
                        .unwrap_or((0.0, FsmPhase::Active));
                    (h.symbol.clone(), (score, phase))
                })
                .collect()
        };
        // P2 — regime label snapshot (market #2, taken alone and
        // dropped; strictly below journal #6 used by record_trade_open).
        let recon_regime_label: Option<String> = state
            .market
            .lock()
            .ok()
            .map(|m| crate::smarttrend::memory_db::regime_label(Some(m.regime)).to_string());
        for h in journal_opens {
            let (fsm_score, fsm_phase) = fsm_scores
                .get(&h.symbol)
                .copied()
                .unwrap_or((0.0, FsmPhase::Active));
            // Prefer the strategy-side score recovered from order_history
            // (Tournament entries); fall back to the live FSM score for
            // legacy momentum / externally-opened positions. This is the
            // fix for all-zero entry_score on recon-materialised strategy
            // trades — the journal now captures real signal strength.
            let entry_score = h.entry_score.unwrap_or(fsm_score);
            record_trade_open(
                &state,
                TradeOpenParams {
                    trade_id: h.trade_id,
                    symbol: h.symbol,
                    side: h.side,
                    entry_price: h.entry_price,
                    qty: h.qty,
                    entry_time: h.entry_time,
                    entry_score,
                    fsm_phase,
                    // v2.5.1 HF — Faza 5 attribution. Previously
                    // hardcoded `None` on the assumption that recon-
                    // discovered opens had no order trail. In practice
                    // a Tournament order whose fill the WebSocket
                    // missed still leaves a fingerprint in
                    // `trading.order_history`. We recover from there
                    // and propagate the canonical strategy id string.
                    // Falls back to `None` only when no matching order
                    // is found (genuine externally-opened position).
                    strategy_id: h.strategy_id,
                    timeframe: h.entry_timeframe,
                    // Recon-materialised opens have no PRIME trail —
                    // the fused conviction lives on the (missed)
                    // PendingOrder, not in order_history.
                    conviction: None,
                    // P2 — recon runs every ~15-30s, so the CURRENT
                    // regime is a faithful proxy for the regime at the
                    // (recently missed) fill. Historical regimes are
                    // not recoverable from order_history.
                    regime: recon_regime_label.clone(),
                },
            );
        }
        for h in journal_closes {
            // S17.3 — fetch actual SELL fills for this symbol from
            // /v2/account/activities/FILL?date=today, recompute
            // realized P/L from real fill prices instead of the stale
            // `unrealized_pnl` snapshot. The HTTP call is made AFTER
            // releasing the trading lock to keep the lock window tiny.
            //
            // Live evidence (2026-05-05):
            //   - journal -$32.57 vs broker FIFO -$359.19 (×11 drift)
            //   - wallet -$2,099 vs broker -$491.86 (×4 drift)
            // Both rooted in `existing.unrealized_pnl` being a stale
            // pre-close snapshot when the bracket-leg fills landed.
            //
            // Fallback chain:
            //   1. fetch_today_realized_exit returns Some(avg_exit, qty)
            //      → realized_pnl = (avg_exit - avg_entry) * qty
            //   2. fetcher fails (network / API down / no fills yet)
            //      → fall back to h.pnl (legacy unrealized_pnl path)
            let (exit_price_effective, pnl_effective, fill_source) =
                match fetch_today_realized_exit(&state, &h.symbol).await {
                    Some((avg_exit, sold_qty)) if sold_qty > 0.0 && h.qty > 0.0 => {
                        // Use the smaller of (broker qty, today's sold
                        // qty) to handle the rare case where today's
                        // fills include partial exits from a position
                        // that was already partially exited yesterday.
                        let match_qty = h.qty.min(sold_qty);
                        let pnl = realized_pnl_simple(h.avg_entry, avg_exit, match_qty);
                        (avg_exit, pnl, "fills")
                    }
                    _ => (h.exit_price, h.pnl, "snapshot"),
                };

            if (pnl_effective - h.pnl).abs() > 0.01 {
                tracing::info!(
                    component = "position_recon",
                    symbol = %h.symbol,
                    pnl_snapshot = h.pnl,
                    pnl_actual = pnl_effective,
                    source = fill_source,
                    "Realized P/L corrected from broker fills"
                );
            }

            // P0-B (audit 2026-05-29) — classify the real exit reason
            // from the captured bracket levels instead of "BrokerSync".
            let exit_reason = classify_exit_reason(
                exit_price_effective,
                h.stop_loss,
                h.take_profit,
                h.opened_at,
                pnl_effective,
            );

            let actually_closed = record_trade_close(
                &state,
                &h.trade_id,
                &h.symbol,
                exit_price_effective,
                pnl_effective,
                exit_reason,
            );

            // S16.5: When trade_updates websocket misses a fill (observed
            // in production: 0 fill events received over many sessions),
            // position_recon is the only path that detects the close. The
            // wallet credit is gated on `actually_closed` because the
            // journal flip is the canonical "close happened" signal — if
            // a late-arriving websocket fill ever does the same close, it
            // will see the entry already closed and bail out.
            if actually_closed {
                let return_amount =
                    Decimal::from_f64(h.invested + pnl_effective).unwrap_or(Decimal::ZERO);
                let return_amount = if return_amount > Decimal::ZERO {
                    return_amount
                } else {
                    Decimal::ZERO
                };
                // v2.5.0 — Tournament strategies use `s2:{strategy_id}` source
                // prefixes (e.g. `s2:catalyst_frontrun`). Without this branch
                // the `match attributed` falls through to the "Unknown
                // position source" warn-and-skip path, leaking the close-out
                // refund. Validate via parse_strategy_from_source so only
                // recognised strategy ids route to bot_wallet; unknown
                // suffixes (`s2:bogus`) still surface as warnings.
                let attributed: &str = if h.source.is_empty() || crate::smarttrend::dplus::parse_strategy_from_source(&h.source).is_some() {
                    "bot"
                } else {
                    h.source.as_str()
                };
                let mut st = state.smarttrend.lock().unwrap_or_else(|p| p.into_inner());
                match attributed {
                    "bot" => {
                        st.bot_wallet.credit(return_amount);
                        tracing::info!(
                            component = "position_recon",
                            symbol = %h.symbol,
                            raw_source = %h.source,
                            return_amount = %return_amount,
                            new_balance = %st.bot_wallet.balance,
                            "Returned to bot wallet via recon (websocket missed fill)"
                        );
                    }
                    "user" => {
                        st.user_wallet.credit(return_amount);
                        tracing::info!(
                            component = "position_recon",
                            symbol = %h.symbol,
                            return_amount = %return_amount,
                            new_balance = %st.user_wallet.balance,
                            "Returned to user wallet via recon (websocket missed fill)"
                        );
                    }
                    other => {
                        tracing::warn!(
                            component = "position_recon",
                            symbol = %h.symbol,
                            source = %other,
                            "Unknown position source — credit skipped"
                        );
                    }
                }
                drop(st);

                // S17.1 + S18 — feed BOTH the intraday loss-streak
                // and the AutoUniverse blacklist machinery from this
                // realised close. Lock order: smarttrend(3) was just
                // released; the wrapper acquires trading(4), config(1),
                // market(2) — all Tier-A — without violating the
                // ascending rule. `h.invested` is the entry-side
                // dollar allocation already cached in the close hint.
                // S17.3 — use corrected pnl_effective (not stale h.pnl)
                // so the streak/blacklist reflect real broker outcomes.
                crate::ops::journal::record_trade_outcome(
                    &state,
                    &h.symbol,
                    pnl_effective,
                    h.invested,
                    Some(&app),
                );

                // D-PLUS (S2) — strategy-attributed close hooks.
                // `h.source` is preserved through reconciliation. Legacy
                // sources ("bot", "user", empty) are no-ops.
                if let Some(dplus) = state.dplus() {
                    let now_ts = chrono::Utc::now().timestamp();
                    let pnl_dec = Decimal::from_f64(pnl_effective).unwrap_or(Decimal::ZERO);
                    let current_regime = {
                        state.market.lock().unwrap().regime
                    };
                    if dplus
                        .on_trade_close(&h.symbol, &h.source, pnl_dec, now_ts, current_regime)
                        .is_some()
                    {
                        let _ = app.emit_all(
                            "dplus-trade-close",
                            serde_json::json!({
                                "symbol": h.symbol,
                                "pnl": pnl_effective,
                                "ts": now_ts,
                                "via": "recon",
                            }),
                        );
                    }
                }

                let _ = app.emit_all("wallet-changed", serde_json::json!({}));
            }
        }

        let new_count = local_positions.len();
        if new_count != last_count {
            let _ = app.emit_all(
                "log-message",
                format!(
                    "RECONCILIATION: {} open positions synced from Alpaca",
                    new_count
                ),
            );
            last_count = new_count;
        }

        let _ = app.emit_all(
            "positions-sync",
            serde_json::json!({
                "count": new_count,
                "positions": local_positions.iter().map(|p| serde_json::json!({
                    "symbol": p.symbol,
                    "qty": p.qty.to_string(),
                    "avg_entry": p.avg_entry.to_string(),
                    "current_price": p.current_price,
                    "unrealized_pnl": p.unrealized_pnl,
                    "side": format!("{:?}", p.side),
                })).collect::<Vec<_>>(),
            }),
        );
    }

    tracing::info!(component = "position_recon", "Position reconciliation loop exiting");
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use rust_decimal_macros::dec;

    // ── S17.3 — realized P/L computation ──

    #[test]
    fn realized_pnl_basic_long_winner() {
        // Bought 100 @ $10, sold @ $11 → +$100
        assert!((realized_pnl_simple(10.0, 11.0, 100.0) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn realized_pnl_basic_long_loser() {
        // Bought 100 @ $10, sold @ $9.50 → -$50
        assert!((realized_pnl_simple(10.0, 9.50, 100.0) + 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn realized_pnl_handles_partial_fills_via_avg() {
        // 100 sold: 50 @ $10.10 + 50 @ $10.30 → avg_exit = $10.20
        // Bought 100 @ $10.00 → P/L = (10.20 - 10.00) * 100 = $20
        let avg_exit = (50.0 * 10.10 + 50.0 * 10.30) / 100.0;
        let pnl = realized_pnl_simple(10.0, avg_exit, 100.0);
        assert!((pnl - 20.0).abs() < 1e-9, "got {pnl}");
    }

    #[test]
    fn realized_pnl_zero_qty_returns_zero() {
        assert_eq!(realized_pnl_simple(10.0, 11.0, 0.0), 0.0);
    }

    #[test]
    fn realized_pnl_handles_non_finite_inputs() {
        assert_eq!(realized_pnl_simple(f64::NAN, 11.0, 100.0), 0.0);
        assert_eq!(realized_pnl_simple(10.0, f64::INFINITY, 100.0), 0.0);
        assert_eq!(realized_pnl_simple(10.0, 11.0, f64::NAN), 0.0);
    }

    #[test]
    fn realized_pnl_breakeven_when_exit_equals_entry() {
        assert!((realized_pnl_simple(15.50, 15.50, 200.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn alpaca_fill_activity_parse() {
        let json = r#"[
            {"symbol":"FATE","side":"sell","qty":"50","price":"5.50","transaction_time":"2026-05-05T17:30:00Z"},
            {"symbol":"FATE","side":"sell","qty":"50","price":"5.70","transaction_time":"2026-05-05T17:31:00Z"},
            {"symbol":"DARE","side":"buy","qty":"100","price":"3.00","transaction_time":"2026-05-05T17:00:00Z"}
        ]"#;
        let fills: Vec<AlpacaFillActivity> = serde_json::from_str(json).unwrap();
        assert_eq!(fills.len(), 3);
        // Replicate the avg-exit calculation in fetch_today_realized_exit.
        let needle = "FATE";
        let mut total_qty = 0.0;
        let mut weighted = 0.0;
        for f in &fills {
            if !f.symbol.eq_ignore_ascii_case(needle) {
                continue;
            }
            if !f.side.eq_ignore_ascii_case("sell") {
                continue;
            }
            let q: f64 = f.qty.parse().unwrap();
            let p: f64 = f.price.parse().unwrap();
            weighted += p * q;
            total_qty += q;
        }
        assert!((total_qty - 100.0).abs() < f64::EPSILON);
        let avg_exit = weighted / total_qty;
        assert!((avg_exit - 5.60).abs() < 1e-9, "avg_exit={avg_exit}");
    }

    #[test]
    fn alpaca_fill_activity_filters_by_side() {
        // Verify BUY fills are not picked up as exits (would inflate
        // realized P/L by ~entry_cost).
        let json = r#"[
            {"symbol":"X","side":"buy","qty":"10","price":"100.0","transaction_time":"t1"},
            {"symbol":"X","side":"sell","qty":"10","price":"101.0","transaction_time":"t2"}
        ]"#;
        let fills: Vec<AlpacaFillActivity> = serde_json::from_str(json).unwrap();
        let mut total_qty = 0.0;
        let mut weighted = 0.0;
        for f in &fills {
            if !f.symbol.eq_ignore_ascii_case("X") {
                continue;
            }
            if !f.side.eq_ignore_ascii_case("sell") {
                continue;
            }
            let q: f64 = f.qty.parse().unwrap();
            let p: f64 = f.price.parse().unwrap();
            weighted += p * q;
            total_qty += q;
        }
        assert!((total_qty - 10.0).abs() < f64::EPSILON);
        let avg_exit = weighted / total_qty;
        assert!((avg_exit - 101.0).abs() < f64::EPSILON);
    }

    #[test]
    fn alpaca_position_parse() {
        let json = r#"[
            {
                "symbol": "AAPL",
                "qty": "10.5",
                "side": "long",
                "avg_entry_price": "180.25",
                "current_price": "185.00",
                "unrealized_pl": "49.88"
            },
            {
                "symbol": "MSFT",
                "qty": "-5",
                "side": "short",
                "avg_entry_price": "420.00",
                "current_price": "415.00",
                "unrealized_pl": "25.00"
            }
        ]"#;

        let positions: Vec<AlpacaPosition> = serde_json::from_str(json).unwrap();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].symbol, "AAPL");
        assert_eq!(positions[1].side, "short");
    }

    #[test]
    fn reconcile_preserves_opened_at() {
        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions.push(Position {
                symbol: "AAPL".into(),
                qty: dec!(10),
                avg_entry: dec!(180),
                current_price: 182.0,
                unrealized_pnl: 20.0,
                side: Direction::Long,
                opened_at: Some(1700000000),
                source: String::new(),
                first_seen: 1700000000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        let mut new_positions = vec![Position {
            symbol: "AAPL".into(),
            qty: dec!(10),
            avg_entry: dec!(180),
            current_price: 185.0,
            unrealized_pnl: 50.0,
            side: Direction::Long,
            opened_at: None,
            source: String::new(),
            first_seen: 1700000099,
            version: 0,
            trade_id: String::new(),
            is_swing_intent: false,
            entry_timeframe: String::new(),
            stop_loss: 0.0,
            take_profit: 0.0,
        }];

        {
            let mut trading = state.trading.lock().unwrap();
            for new_pos in &mut new_positions {
                if let Some(existing) = trading
                    .positions
                    .iter()
                    .find(|p| p.symbol == new_pos.symbol)
                {
                    new_pos.opened_at = existing.opened_at.or(Some(existing.first_seen));
                    new_pos.first_seen = existing.first_seen;
                }
            }
            trading.positions = new_positions;
        }

        let trading = state.trading.lock().unwrap();
        assert_eq!(trading.positions.len(), 1);
        assert_eq!(trading.positions[0].opened_at, Some(1700000000));
        assert_eq!(trading.positions[0].first_seen, 1700000000);
        assert!((trading.positions[0].current_price - 185.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reconcile_backfills_opened_at_from_first_seen() {
        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions.push(Position {
                symbol: "TSLA".into(),
                qty: dec!(3),
                avg_entry: dec!(200),
                current_price: 205.0,
                unrealized_pnl: 15.0,
                side: Direction::Long,
                opened_at: None,
                source: String::new(),
                first_seen: 1700000050,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        let mut new_positions = vec![Position {
            symbol: "TSLA".into(),
            qty: dec!(3),
            avg_entry: dec!(200),
            current_price: 210.0,
            unrealized_pnl: 30.0,
            side: Direction::Long,
            opened_at: None,
            source: String::new(),
            first_seen: 1700000099,
            version: 0,
            trade_id: String::new(),
            is_swing_intent: false,
            entry_timeframe: String::new(),
            stop_loss: 0.0,
            take_profit: 0.0,
        }];

        {
            let mut trading = state.trading.lock().unwrap();
            for new_pos in &mut new_positions {
                if let Some(existing) = trading
                    .positions
                    .iter()
                    .find(|p| p.symbol == new_pos.symbol)
                {
                    new_pos.opened_at = existing.opened_at.or(Some(existing.first_seen));
                    new_pos.first_seen = existing.first_seen;
                }
            }
            trading.positions = new_positions;
        }

        let trading = state.trading.lock().unwrap();
        assert_eq!(trading.positions[0].opened_at, Some(1700000050));
        assert_eq!(trading.positions[0].first_seen, 1700000050);
    }

    #[test]
    fn reconcile_new_position_no_opened_at() {
        let state = TitanState::new(RuntimeConfig::default());

        let new_positions = vec![Position {
            symbol: "NVDA".into(),
            qty: dec!(5),
            avg_entry: dec!(800),
            current_price: 810.0,
            unrealized_pnl: 50.0,
            side: Direction::Long,
            opened_at: None,
            source: String::new(),
            first_seen: 1700000000,
            version: 0,
            trade_id: String::new(),
            is_swing_intent: false,
            entry_timeframe: String::new(),
            stop_loss: 0.0,
            take_profit: 0.0,
        }];

        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions = new_positions;
        }

        let trading = state.trading.lock().unwrap();
        assert_eq!(trading.positions.len(), 1);
        assert!(trading.positions[0].opened_at.is_none());
        assert_eq!(trading.positions[0].first_seen, 1700000000);
    }

    #[test]
    fn reconcile_removes_closed_positions() {
        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions.push(Position {
                symbol: "AAPL".into(),
                qty: dec!(10),
                avg_entry: dec!(180),
                current_price: 182.0,
                unrealized_pnl: 20.0,
                side: Direction::Long,
                opened_at: Some(1000),
                source: String::new(),
                first_seen: 1000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
            trading.positions.push(Position {
                symbol: "MSFT".into(),
                qty: dec!(5),
                avg_entry: dec!(400),
                current_price: 405.0,
                unrealized_pnl: 25.0,
                side: Direction::Long,
                opened_at: Some(2000),
                source: String::new(),
                first_seen: 2000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        let new_positions = vec![Position {
            symbol: "AAPL".into(),
            qty: dec!(10),
            avg_entry: dec!(180),
            current_price: 185.0,
            unrealized_pnl: 50.0,
            side: Direction::Long,
            opened_at: None,
            source: String::new(),
            first_seen: 9999,
            version: 0,
            trade_id: String::new(),
            is_swing_intent: false,
            entry_timeframe: String::new(),
            stop_loss: 0.0,
            take_profit: 0.0,
        }];

        {
            let mut trading = state.trading.lock().unwrap();
            let mut merged = new_positions;
            for pos in &mut merged {
                if let Some(existing) = trading
                    .positions
                    .iter()
                    .find(|p| p.symbol == pos.symbol)
                {
                    pos.opened_at = existing.opened_at.or(Some(existing.first_seen));
                    pos.first_seen = existing.first_seen;
                }
            }
            trading.positions = merged;
        }

        let trading = state.trading.lock().unwrap();
        assert_eq!(trading.positions.len(), 1);
        assert_eq!(trading.positions[0].symbol, "AAPL");
        assert_eq!(trading.positions[0].opened_at, Some(1000));
        assert_eq!(trading.positions[0].first_seen, 1000);
    }

    #[test]
    fn recon_skips_qty_overwrite_when_version_changed() {
        use std::collections::HashMap;

        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions.push(Position {
                symbol: "AAPL".into(),
                qty: dec!(10),
                avg_entry: dec!(180),
                current_price: 182.0,
                unrealized_pnl: 20.0,
                side: Direction::Long,
                opened_at: Some(1000),
                source: "bot".into(),
                first_seen: 1000,
                version: 0,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        // Snapshot version before "fetch"
        let pre_versions: HashMap<String, u64> = {
            let trading = state.trading.lock().unwrap();
            trading.positions.iter().map(|p| (p.symbol.clone(), p.version)).collect()
        };

        // Simulate trade_updates bumping version during the HTTP round-trip
        {
            let mut trading = state.trading.lock().unwrap();
            let pos = trading.positions.iter_mut().find(|p| p.symbol == "AAPL").unwrap();
            pos.qty = dec!(15);
            pos.avg_entry = dec!(183);
            pos.version = pos.version.wrapping_add(1);
        }

        // Now simulate the CAS-merge (same logic as production)
        {
            let mut trading = state.trading.lock().unwrap();
            let existing = &trading.positions[0];
            let pre_ver = pre_versions.get(&existing.symbol).copied().unwrap_or(0);
            assert_ne!(existing.version, pre_ver, "version should have changed");

            // Broker says qty=10, avg=180 (stale vs trade_updates' 15/183).
            // CAS-merge should keep local qty/avg_entry.
            let mut pos = existing.clone();
            pos.current_price = 190.0;
            pos.unrealized_pnl = 105.0;
            trading.positions = vec![pos];
        }

        let trading = state.trading.lock().unwrap();
        assert_eq!(trading.positions[0].qty, dec!(15), "qty must be preserved from trade_updates");
        assert_eq!(trading.positions[0].avg_entry, dec!(183));
        assert!((trading.positions[0].current_price - 190.0).abs() < f64::EPSILON);
    }

    #[test]
    fn recon_refreshes_current_price_when_version_changed() {
        use std::collections::HashMap;

        let state = TitanState::new(RuntimeConfig::default());

        {
            let mut trading = state.trading.lock().unwrap();
            trading.positions.push(Position {
                symbol: "NVDA".into(),
                qty: dec!(5),
                avg_entry: dec!(800),
                current_price: 810.0,
                unrealized_pnl: 50.0,
                side: Direction::Long,
                opened_at: Some(2000),
                source: "user".into(),
                first_seen: 2000,
                version: 3,
                trade_id: String::new(),
                is_swing_intent: false,
                entry_timeframe: String::new(),
                stop_loss: 0.0,
                take_profit: 0.0,
            });
        }

        let pre_versions: HashMap<String, u64> = {
            let trading = state.trading.lock().unwrap();
            trading.positions.iter().map(|p| (p.symbol.clone(), p.version)).collect()
        };

        // trade_updates bumps version
        {
            let mut trading = state.trading.lock().unwrap();
            let pos = trading.positions.iter_mut().find(|p| p.symbol == "NVDA").unwrap();
            pos.qty = dec!(8);
            pos.version = pos.version.wrapping_add(1);
        }

        // CAS-merge: broker has current_price=820 — should update that
        {
            let mut trading = state.trading.lock().unwrap();
            let existing = &trading.positions[0];
            let pre_ver = pre_versions.get(&existing.symbol).copied().unwrap_or(0);
            assert_ne!(existing.version, pre_ver);

            let mut pos = existing.clone();
            pos.current_price = 820.0;
            pos.unrealized_pnl = 160.0;
            trading.positions = vec![pos];
        }

        let trading = state.trading.lock().unwrap();
        assert_eq!(trading.positions[0].qty, dec!(8), "qty preserved");
        assert!((trading.positions[0].current_price - 820.0).abs() < f64::EPSILON, "price refreshed");
    }

    // ─── v2.5.1 HF: Faza 5 attribution recovery tests ───────────────────

    fn mk_order(
        symbol: &str,
        side: &str,
        source: &str,
        status: &str,
        submitted_at: i64,
    ) -> crate::state::OrderHistoryEntry {
        crate::state::OrderHistoryEntry {
            order_id: format!("test-{symbol}-{submitted_at}"),
            symbol: symbol.into(),
            side: side.into(),
            qty: dec!(100),
            order_type: "market".into(),
            limit_price: None,
            status: status.into(),
            source: source.into(),
            submitted_at,
            filled_price: None,
            filled_at: None,
            reject_reason: None,
            entry_score: None,
            entry_timeframe: String::new(),
        }
    }

    #[test]
    fn recover_attribution_picks_most_recent_matching_order() {
        // Live 2026-05-18 scenario: bot submits ~10 halt_resume buys
        // for HIVE in premarket; WS misses fills; recon discovers the
        // broker-side Long position. The most recent buy with a
        // non-empty source must be returned.
        let history = vec![
            mk_order("HIVE", "buy", "s2:halt_resume", "pending_new", 100),
            mk_order("HIVE", "buy", "s2:halt_resume", "pending_new", 200),
            mk_order("HIVE", "buy", "s2:halt_resume", "pending_new", 300),
        ];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "HIVE", Direction::Long);
        assert_eq!(source, "s2:halt_resume");
        assert_eq!(strategy_id, Some("halt_resume".to_string()));
    }

    #[test]
    fn recover_entry_score_picks_matching_order_score() {
        // The strategy-side entry score must be recovered from the most
        // recent successful entry-side order, mirroring the attribution
        // recovery. This is the fix that stops recon-materialised
        // strategy trades from journalling a hardcoded 0.0 score.
        let mut o1 = mk_order("HIVE", "buy", "s2:halt_resume", "pending_new", 100);
        o1.entry_score = Some(61.0);
        let mut o2 = mk_order("HIVE", "buy", "s2:halt_resume", "pending_new", 200);
        o2.entry_score = Some(78.5);
        let history = vec![o1, o2];
        let score =
            recover_entry_score_from_order_history(&history, "HIVE", Direction::Long);
        assert_eq!(score, Some(78.5), "must pick the most recent entry order's score");
    }

    #[test]
    fn recover_entry_score_none_when_unscored_or_failed() {
        // Legacy / external orders carry no score → None. A failed order
        // bearing a score must NOT be attributed to a live position.
        let plain = mk_order("DXC", "buy", "s2:halt_resume", "pending_new", 100);
        assert_eq!(
            recover_entry_score_from_order_history(&[plain], "DXC", Direction::Long),
            None,
        );
        let mut failed = mk_order("DXC", "buy", "s2:halt_resume", "rejected", 100);
        failed.entry_score = Some(90.0);
        assert_eq!(
            recover_entry_score_from_order_history(&[failed], "DXC", Direction::Long),
            None,
            "rejected order's score must be ignored",
        );
    }

    #[test]
    fn recover_entry_timeframe_picks_matching_order_tf() {
        // Mirrors entry_score recovery: the TF must come from the most
        // recent successful entry-side order so the journal records which
        // timeframe the entry actually fired on.
        let mut o1 = mk_order("HIVE", "buy", "s2:opening_range_breakout", "pending_new", 100);
        o1.entry_timeframe = "5m".into();
        let mut o2 = mk_order("HIVE", "buy", "s2:opening_range_breakout", "pending_new", 200);
        o2.entry_timeframe = "30m".into();
        let history = vec![o1, o2];
        assert_eq!(
            recover_entry_timeframe_from_order_history(&history, "HIVE", Direction::Long),
            "30m",
            "must pick the most recent entry order's timeframe",
        );
    }

    #[test]
    fn recover_entry_timeframe_empty_when_absent_or_failed() {
        // No TF on the order → empty. A failed order carrying a TF must
        // not be attributed to a live position.
        let plain = mk_order("DXC", "buy", "s2:halt_resume", "pending_new", 100);
        assert!(
            recover_entry_timeframe_from_order_history(&[plain], "DXC", Direction::Long)
                .is_empty(),
        );
        let mut failed = mk_order("DXC", "buy", "s2:halt_resume", "rejected", 100);
        failed.entry_timeframe = "5m".into();
        assert!(
            recover_entry_timeframe_from_order_history(&[failed], "DXC", Direction::Long)
                .is_empty(),
            "rejected order's timeframe must be ignored",
        );
    }

    #[test]
    fn recover_attribution_skips_failed_orders() {
        // Rationale: failed/rejected orders never filled, so the
        // matching broker-side position can NOT have come from them.
        // The recovery must fall through to the most recent
        // non-failed entry — or return None if all are failed.
        let history = vec![
            mk_order("QBTS", "buy", "s2:catalyst_frontrun", "failed", 100),
            mk_order("QBTS", "buy", "s2:catalyst_frontrun", "rejected", 200),
            mk_order("QBTS", "buy", "s2:catalyst_frontrun", "canceled", 300),
        ];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "QBTS", Direction::Long);
        assert!(source.is_empty(), "all-failed history must produce empty source");
        assert!(strategy_id.is_none(), "all-failed history must produce no strategy_id");
    }

    #[test]
    fn recover_attribution_falls_through_failed_to_pending() {
        // Realistic mix: early rejections, later success.
        let history = vec![
            mk_order("SATL", "sell", "s2:catalyst_frontrun", "failed", 100),
            mk_order("SATL", "sell", "s2:extended_news", "rejected", 200),
            mk_order("SATL", "sell", "s2:catalyst_frontrun", "pending_new", 300),
        ];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "SATL", Direction::Short);
        assert_eq!(source, "s2:catalyst_frontrun");
        assert_eq!(strategy_id, Some("catalyst_frontrun".to_string()));
    }

    #[test]
    fn recover_attribution_requires_entry_side() {
        // For a Long position, exit-side `sell` orders MUST NOT be
        // picked up. Otherwise a re-opened position after a
        // close-then-re-enter sequence would be attributed to the
        // wrong (close) order.
        let history = vec![
            mk_order("RDW", "sell", "s2:gap_fade", "pending_new", 100), // close-side
            mk_order("RDW", "buy", "s2:gap_fade", "pending_new", 50),   // entry-side, earlier
        ];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "RDW", Direction::Long);
        assert_eq!(source, "s2:gap_fade", "must pick the buy (entry) for Long");
        assert_eq!(strategy_id, Some("gap_fade".to_string()));
    }

    #[test]
    fn recover_attribution_short_position_needs_sell_order() {
        // Symmetric guard: a Short position is opened by a SELL order.
        // The recovery must NOT pick up an earlier buy on the same
        // symbol (would be a long-side close or an unrelated entry).
        let history = vec![
            mk_order("TRT", "buy", "s2:halt_resume", "pending_new", 50),   // not for short
            mk_order("TRT", "sell", "s2:halt_resume", "pending_new", 100), // short entry
        ];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "TRT", Direction::Short);
        assert_eq!(source, "s2:halt_resume");
        assert_eq!(strategy_id, Some("halt_resume".to_string()));
    }

    #[test]
    fn recover_attribution_unknown_source_yields_no_strategy() {
        // Manual orders (`user` prefix) or legacy bot trades (`bot`)
        // are not Tournament strategies. The source field is
        // preserved verbatim, but `strategy_id` must remain None so
        // Faza 5 attribution doesn't claim them as Tournament wins.
        let history = vec![mk_order(
            "AAPL",
            "buy",
            "user-manual-order",
            "pending_new",
            100,
        )];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "AAPL", Direction::Long);
        assert_eq!(source, "user-manual-order");
        assert!(
            strategy_id.is_none(),
            "non-s2 sources must not yield a strategy_id"
        );
    }

    #[test]
    fn recover_attribution_empty_history_returns_blank() {
        let history: Vec<crate::state::OrderHistoryEntry> = vec![];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "ANY", Direction::Long);
        assert!(source.is_empty());
        assert!(strategy_id.is_none());
    }

    #[test]
    fn recover_attribution_case_insensitive_symbol_and_side() {
        // Tolerance for vendor inconsistency: Alpaca sometimes
        // returns uppercase symbols, our local order log normalises
        // however it likes, etc. The match must be robust.
        let history = vec![mk_order(
            "hive",
            "BUY",
            "s2:halt_resume",
            "pending_new",
            100,
        )];
        let (source, strategy_id) =
            recover_attribution_from_order_history(&history, "HIVE", Direction::Long);
        assert_eq!(source, "s2:halt_resume");
        assert_eq!(strategy_id, Some("halt_resume".to_string()));
    }
}
