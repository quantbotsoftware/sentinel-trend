//! Pure simulator: feeds a strategy implementing the `Strategy` trait
//! with bars one at a time, opens/closes simulated positions, and
//! emits a trade ledger plus equity curve. No Tauri, no I/O, fully
//! deterministic — R4 invariant.
//!
//! ## Flow per bar `i`
//!
//! 1. Build the `MarketContext` from `bars[..=i]`.
//! 2. If a position is open:
//!    - **Intrabar bracket check** on the CURRENT bar's OHL using the
//!      SL/TP captured at entry. If hit → close at the slipped bracket
//!      price.
//!    - Else, call `strategy.evaluate_exit(pos, ctx)`. If `Some`,
//!      close at the bar's close (with slippage).
//!    - Else, enforce simulator-side max-hold (bars count) as a
//!      safety rail independent of the strategy's own time gate.
//! 3. If FLAT:
//!    - Honour `strategy.allowed_window()` against the ET clock.
//!    - Call `strategy.evaluate_entry(ctx)`. If `Some` and the
//!      strategy returns size > 0, open at bar close + slippage.
//!
//! ## Time-travel trick for `evaluate_exit`
//!
//! Strategies call `chrono::Utc::now().timestamp()` to compute hold
//! duration — useful live, wrong offline. The simulator rewrites the
//! synthetic `Position.opened_at` each bar so the (wall-clock - opened_at)
//! delta equals the simulation's elapsed bar time. This lets each
//! strategy's own `max_hold_minutes` time-stop fire authentically
//! without changing the trait surface.

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::Serialize;
use titan_types::Candle;

use crate::smarttrend::strategies::{Strategy, StrategyId};
use crate::state::{Direction, Position};

use super::context_builder::{build_context, ContextDefaults};
use super::fills::{resolve_bracket, BracketFill, FillModel};

/// One closed trade — entry + exit pair with cost-corrected P/L.
#[derive(Debug, Clone, Serialize)]
pub struct SimTrade {
    pub strategy: StrategyId,
    pub symbol: String,
    pub side: Direction,
    pub entry_time: i64,
    pub exit_time: i64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub qty: f64,
    pub gross_pnl: f64,
    pub fees: f64,
    pub net_pnl: f64,
    pub exit_reason: String,
    pub bars_held: u32,
}

/// Per-bar mark of total simulator equity (cash + open MTM).
#[derive(Debug, Clone, Serialize)]
pub struct EquityPoint {
    pub time: i64,
    pub equity: f64,
}

/// Full output of `simulate` for one (strategy, symbol) pair.
#[derive(Debug, Clone, Serialize)]
pub struct SimReport {
    pub strategy: StrategyId,
    pub symbol: String,
    pub timeframe: String,
    pub trades: Vec<SimTrade>,
    pub equity_curve: Vec<EquityPoint>,
    pub initial_equity: f64,
    pub final_equity: f64,
    pub bars_processed: usize,
}

/// Inputs the simulator needs beyond the bar slice + strategy ref.
#[derive(Debug, Clone)]
pub struct SimParams {
    pub symbol: String,
    pub timeframe: &'static str,
    pub initial_equity: f64,
    pub fill_model: FillModel,
    /// Number of leading bars to skip before evaluating (warmup for
    /// indicators). Strategies have their own minimum-history guard;
    /// this is a global floor.
    pub warmup_bars: usize,
    /// Realistic constant spread for the simulated bars.
    pub spread_bps: f64,
    /// Optional parameter overrides for the strategy.
    pub overrides: std::collections::HashMap<String, f64>,
}

impl Default for SimParams {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            timeframe: "5m",
            initial_equity: 10_000.0,
            fill_model: FillModel::default(),
            warmup_bars: 30,
            spread_bps: 5.0,
            overrides: std::collections::HashMap::new(),
        }
    }
}

/// Internal open-position state. Distinct from `state::Position` so
/// it owns the bracket fields the simulator cares about (the live
/// `Position` has them too but is heavier).
struct OpenPos {
    side: Direction,
    qty: f64,
    entry_price: f64,
    entry_time: i64,
    entry_bar_idx: usize,
    stop_loss: f64,
    take_profit: f64,
    max_hold_minutes: u32,
    entry_fees: f64,
}

/// Run a single-symbol simulation for one strategy. Pure function:
/// same inputs → same outputs, no I/O.
pub fn simulate(strategy: &dyn Strategy, bars: &[Candle], params: &SimParams) -> SimReport {
    let mut report = SimReport {
        strategy: strategy.id(),
        symbol: params.symbol.clone(),
        timeframe: params.timeframe.to_string(),
        trades: Vec::new(),
        equity_curve: Vec::with_capacity(bars.len()),
        initial_equity: params.initial_equity,
        final_equity: params.initial_equity,
        bars_processed: 0,
    };

    let ctx_defaults = ContextDefaults {
        symbol: params.symbol.clone(),
        timeframe: params.timeframe,
        spread_bps: params.spread_bps,
        regime: Some(crate::smarttrend::strategies::MarketRegime::Calm),
    };

    let mut cash = params.initial_equity;
    let mut open: Option<OpenPos> = None;
    let allowed = strategy.allowed_window();

    for i in params.warmup_bars..bars.len() {
        let bar = &bars[i];
        report.bars_processed += 1;

        let mut ctx = match build_context(&bars[..=i], &ctx_defaults) {
            Some(c) => c,
            None => continue,
        };
        ctx.overrides.insert(strategy.id(), params.overrides.clone());

        // ─── Handle open position ──────────────────────────────
        if let Some(pos) = open.as_ref() {
            // 1. Bracket check on this bar's OHL (the bar opened
            //    after the entry bar; SL/TP can fire intrabar).
            if i > pos.entry_bar_idx {
                let bracket = resolve_bracket(
                    pos.side,
                    pos.stop_loss,
                    pos.take_profit,
                    bar.low,
                    bar.high,
                    &params.fill_model,
                );
                match bracket {
                    BracketFill::StopLoss(price) => {
                        cash = close_trade(
                            &mut report,
                            pos,
                            price,
                            bar.time,
                            i,
                            "bracket_stop_loss",
                            cash,
                            &params.fill_model,
                        );
                        open = None;
                    }
                    BracketFill::TakeProfit(price) => {
                        cash = close_trade(
                            &mut report,
                            pos,
                            price,
                            bar.time,
                            i,
                            "bracket_take_profit",
                            cash,
                            &params.fill_model,
                        );
                        open = None;
                    }
                    BracketFill::None => {}
                }
            }

            // 2. Strategy-level evaluate_exit (only if bracket didn't fire).
            if let Some(pos_ref) = open.as_ref() {
                let synthetic_pos = synth_position(pos_ref, bar.time);
                if let Some(sig) = strategy.evaluate_exit(&synthetic_pos, &ctx) {
                    let exit_price =
                        params.fill_model.slipped(bar.close, pos_ref.side, false);
                    cash = close_trade(
                        &mut report,
                        pos_ref,
                        exit_price,
                        bar.time,
                        i,
                        sig.reason,
                        cash,
                        &params.fill_model,
                    );
                    open = None;
                }
            }

            // 3. Simulator-side max-hold rail (bars-based, independent
            //    of strategy `chrono::Utc::now()` shenanigans).
            if let Some(pos_ref) = open.as_ref() {
                let bars_held = (i - pos_ref.entry_bar_idx) as u32;
                let tf_secs = timeframe_secs(params.timeframe);
                let minutes_held = bars_held as i64 * (tf_secs as i64) / 60;
                if pos_ref.max_hold_minutes > 0
                    && minutes_held >= pos_ref.max_hold_minutes as i64
                {
                    let exit_price =
                        params.fill_model.slipped(bar.close, pos_ref.side, false);
                    cash = close_trade(
                        &mut report,
                        pos_ref,
                        exit_price,
                        bar.time,
                        i,
                        "sim_max_hold",
                        cash,
                        &params.fill_model,
                    );
                    open = None;
                }
            }
        }

        // ─── Look for new entry ────────────────────────────────
        if open.is_none() && allowed.permits(ctx.now_et_hour, ctx.now_et_minute) {
            if let Some(entry) = strategy.evaluate_entry(&ctx) {
                let dollars = strategy.position_size(&ctx, cash);
                if dollars > 0.0 && ctx.price > 0.0 {
                    let raw_qty = (dollars / ctx.price).floor();
                    if raw_qty >= 1.0 {
                        let qty = raw_qty;
                        let fill_price =
                            params.fill_model.slipped(ctx.price, entry.side, true);
                        let entry_fees = params.fill_model.fee(qty);
                        cash -= entry_fees;
                        // No notional debit/credit at entry — P/L is
                        // realized at exit. Cash tracks closed P/L
                        // minus fees so the equity curve is monotonic
                        // in the absence of trades.
                        open = Some(OpenPos {
                            side: entry.side,
                            qty,
                            entry_price: fill_price,
                            entry_time: bar.time,
                            entry_bar_idx: i,
                            stop_loss: entry.stop_loss,
                            take_profit: entry.take_profit,
                            max_hold_minutes: entry.max_hold_minutes,
                            entry_fees,
                        });
                    }
                }
            }
        }

        // Mark equity (cash + unrealized).
        let unrealized = match open.as_ref() {
            Some(p) => match p.side {
                Direction::Long => (bar.close - p.entry_price) * p.qty,
                Direction::Short => (p.entry_price - bar.close) * p.qty,
            },
            None => 0.0,
        };
        report.equity_curve.push(EquityPoint {
            time: bar.time,
            equity: cash + unrealized,
        });
    }

    // Force-close any straggling position at the last bar close
    // so the metrics reflect closed equity.
    if let Some(pos) = open.as_ref() {
        if let Some(last) = bars.last() {
            let exit_price = params.fill_model.slipped(last.close, pos.side, false);
            cash = close_trade(
                &mut report,
                pos,
                exit_price,
                last.time,
                bars.len().saturating_sub(1),
                "sim_force_flat",
                cash,
                &params.fill_model,
            );
        }
    }

    report.final_equity = cash;
    report
}

#[allow(clippy::too_many_arguments)]
fn close_trade(
    report: &mut SimReport,
    pos: &OpenPos,
    exit_price: f64,
    exit_time: i64,
    exit_bar_idx: usize,
    reason: &str,
    cash_in: f64,
    fill_model: &FillModel,
) -> f64 {
    let gross = match pos.side {
        Direction::Long => (exit_price - pos.entry_price) * pos.qty,
        Direction::Short => (pos.entry_price - exit_price) * pos.qty,
    };
    let exit_fees = fill_model.fee(pos.qty);
    let total_fees = pos.entry_fees + exit_fees;
    let net = gross - exit_fees; // entry_fees already debited from cash at entry
    let cash_out = cash_in + net;
    let bars_held = exit_bar_idx.saturating_sub(pos.entry_bar_idx) as u32;
    report.trades.push(SimTrade {
        strategy: report.strategy,
        symbol: report.symbol.clone(),
        side: pos.side,
        entry_time: pos.entry_time,
        exit_time,
        entry_price: pos.entry_price,
        exit_price,
        qty: pos.qty,
        gross_pnl: gross,
        fees: total_fees,
        net_pnl: gross - total_fees,
        exit_reason: reason.to_string(),
        bars_held,
    });
    cash_out
}

/// Construct a synthetic `Position` to hand to `evaluate_exit`. The
/// `opened_at` field is rewritten relative to wall-clock so the
/// strategy's `chrono::Utc::now() - opened_at` math reflects the
/// simulation's elapsed time, not wall-clock since simulation start.
fn synth_position(pos: &OpenPos, bar_time: i64) -> Position {
    let elapsed_secs = (bar_time - pos.entry_time).max(0);
    let virtual_opened_at = Utc::now().timestamp() - elapsed_secs;
    Position {
        symbol: String::new(),
        qty: Decimal::from_f64(pos.qty).unwrap_or(Decimal::ZERO),
        avg_entry: Decimal::from_f64(pos.entry_price).unwrap_or(Decimal::ZERO),
        current_price: pos.entry_price,
        unrealized_pnl: 0.0,
        side: pos.side,
        opened_at: Some(virtual_opened_at),
        source: String::new(),
        first_seen: virtual_opened_at,
        version: 0,
        trade_id: String::new(),
        is_swing_intent: false,
        entry_timeframe: String::new(),
        stop_loss: pos.stop_loss,
        take_profit: pos.take_profit,
    }
}

fn timeframe_secs(tf: &str) -> u64 {
    match tf {
        "1m" => 60,
        "5m" => 300,
        "10m" => 600,
        "15m" => 900,
        "30m" => 1800,
        "1h" => 3600,
        "4h" => 14_400,
        "1d" | "1D" => 86_400,
        _ => 300,
    }
}

// Tests live in a sibling file to keep `engine.rs` under the
// R2 LOC cap. See module docs in `engine_tests.rs`.
#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
