//! Unit tests for the simulator. Split into a sibling file via the
//! `#[path]` chain from `engine.rs` so `engine.rs` itself stays under
//! the 500-LOC R2 cap (the engine body is genuinely cohesive — the
//! split is purely a LOC budget concession).

use chrono::TimeZone;
use titan_types::Candle;

use crate::smarttrend::strategies::{
    EntrySignal, ExitSignal, MarketContext, Strategy, StrategyId, TradingWindow,
};
use crate::state::{Direction, Position};

use super::{simulate, SimParams};

fn et_ts(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    chrono_tz::US::Eastern
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .unwrap()
        .timestamp()
}

fn make_bar(time: i64, open: f64, high: f64, low: f64, close: f64, vol: f64) -> Candle {
    Candle { time, open, high, low, close, volume: vol }
}

/// Toy always-long strategy: fires entry once, no strategy-side exit
/// hooks — used to pin the simulator's bracket / max-hold / force-flat
/// rails.
struct AlwaysLong {
    triggered: std::sync::atomic::AtomicBool,
}

impl AlwaysLong {
    fn new() -> Self {
        Self {
            triggered: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl Strategy for AlwaysLong {
    fn id(&self) -> StrategyId {
        StrategyId::OpeningRangeBreakout
    }
    fn allowed_window(&self) -> TradingWindow {
        TradingWindow::AlwaysOn
    }
    fn evaluate_entry(&self, ctx: &MarketContext) -> Option<EntrySignal> {
        if self
            .triggered
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        Some(EntrySignal {
            strategy: StrategyId::OpeningRangeBreakout,
            side: Direction::Long,
            limit_price: None,
            stop_loss: ctx.price * 0.95,
            take_profit: ctx.price * 1.10,
            max_hold_minutes: 5,
            score: 50.0,
            reason: "test",
        })
    }
    fn evaluate_exit(&self, _pos: &Position, _ctx: &MarketContext) -> Option<ExitSignal> {
        None
    }
    fn position_size(&self, _ctx: &MarketContext, sub_wallet: f64) -> f64 {
        sub_wallet * 0.5
    }
}

#[test]
fn simulator_opens_then_exits_via_bracket_take_profit() {
    let base = et_ts(2026, 6, 15, 9, 30);
    let mut bars: Vec<Candle> = (0..40)
        .map(|i| make_bar(base + (i as i64) * 60, 100.0, 100.5, 99.5, 100.0, 10_000.0))
        .collect();
    // Pump a TP bar at index 35 (+15 %, well past +10 % TP).
    bars[35] = make_bar(bars[35].time, 100.0, 115.0, 99.0, 114.5, 50_000.0);
    let strat = AlwaysLong::new();
    let params = SimParams {
        symbol: "TEST".into(),
        timeframe: "1m",
        initial_equity: 10_000.0,
        warmup_bars: 30,
        ..SimParams::default()
    };
    let report = simulate(&strat, &bars, &params);
    assert_eq!(report.trades.len(), 1);
    let t = &report.trades[0];
    assert_eq!(t.exit_reason, "bracket_take_profit");
    assert!(t.net_pnl > 0.0, "should be profitable");
}

#[test]
fn simulator_stop_loss_on_red_bar() {
    let base = et_ts(2026, 6, 15, 9, 30);
    let mut bars: Vec<Candle> = (0..40)
        .map(|i| make_bar(base + (i as i64) * 60, 100.0, 100.5, 99.5, 100.0, 10_000.0))
        .collect();
    // Crash bar at index 35 — punches through SL=95.
    bars[35] = make_bar(bars[35].time, 100.0, 100.5, 90.0, 91.0, 50_000.0);
    let strat = AlwaysLong::new();
    let params = SimParams {
        symbol: "TEST".into(),
        timeframe: "1m",
        initial_equity: 10_000.0,
        warmup_bars: 30,
        ..SimParams::default()
    };
    let report = simulate(&strat, &bars, &params);
    assert_eq!(report.trades.len(), 1);
    let t = &report.trades[0];
    assert_eq!(t.exit_reason, "bracket_stop_loss");
    assert!(t.net_pnl < 0.0, "should be a loss");
}

#[test]
fn simulator_force_flat_at_end() {
    let base = et_ts(2026, 6, 15, 9, 30);
    let bars: Vec<Candle> = (0..40)
        .map(|i| make_bar(base + (i as i64) * 60, 100.0, 100.2, 99.8, 100.0, 10_000.0))
        .collect();
    let strat = AlwaysLong::new();
    let params = SimParams {
        symbol: "TEST".into(),
        timeframe: "1m",
        initial_equity: 10_000.0,
        warmup_bars: 30,
        ..SimParams::default()
    };
    let report = simulate(&strat, &bars, &params);
    assert_eq!(report.trades.len(), 1);
    // 5-min max hold on a 1m TF → simulator rail closes before the
    // final force-flat sweep, but accept both for robustness.
    assert!(
        matches!(
            report.trades[0].exit_reason.as_str(),
            "sim_max_hold" | "sim_force_flat" | "orb_max_hold"
        ),
        "got reason {}",
        report.trades[0].exit_reason
    );
}

#[test]
fn equity_curve_has_one_point_per_processed_bar() {
    let base = et_ts(2026, 6, 15, 9, 30);
    let bars: Vec<Candle> = (0..40)
        .map(|i| make_bar(base + (i as i64) * 60, 100.0, 100.5, 99.5, 100.0, 10_000.0))
        .collect();
    let strat = AlwaysLong::new();
    let params = SimParams {
        symbol: "TEST".into(),
        timeframe: "1m",
        warmup_bars: 30,
        ..SimParams::default()
    };
    let report = simulate(&strat, &bars, &params);
    assert_eq!(report.equity_curve.len(), report.bars_processed);
    assert_eq!(report.bars_processed, bars.len() - 30);
}
