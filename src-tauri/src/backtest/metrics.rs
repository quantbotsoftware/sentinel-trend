//! Performance metrics for a `SimReport`.
//!
//! Pure functions over the trade ledger + equity curve; no I/O.
//! Designed so a report from `engine::simulate` can be summarised in
//! O(n) for both trades and equity points.

use serde::Serialize;

use super::engine::{EquityPoint, SimReport, SimTrade};

/// Headline summary statistics for a single backtest run.
///
/// `annualisation_factor` is the assumed number of independent bars
/// per year — used to anneal Sharpe / Sortino from per-bar to annual
/// numbers. Examples:
/// - 1-minute bars over RTH (6.5 h × 60): `390 × 252 = 98_280`
/// - 5-minute bars over RTH: `78 × 252 = 19_656`
/// - Daily bars: `252`
///
/// The annualisation choice is conservative: SP500 RTH only, no
/// extended hours, no overnight gap volatility.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PerfMetrics {
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    pub gross_profit: f64,
    pub gross_loss: f64,
    pub net_pnl: f64,
    pub profit_factor: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub expectancy: f64,
    pub max_drawdown_pct: f64,
    pub max_drawdown_abs: f64,
    pub sharpe: f64,
    pub sortino: f64,
    pub cagr: f64,
    pub exposure_pct: f64,
    pub avg_bars_held: f64,
    pub initial_equity: f64,
    pub final_equity: f64,
}

/// Compute headline metrics from a `SimReport`. `annualisation_factor`
/// is the number of bar-returns per year (see struct doc).
pub fn compute(report: &SimReport, annualisation_factor: f64) -> PerfMetrics {
    let mut m = PerfMetrics {
        initial_equity: report.initial_equity,
        final_equity: report.final_equity,
        ..Default::default()
    };
    let trades = &report.trades;
    m.trades = trades.len();
    if trades.is_empty() {
        // Even with no trades we can still report the equity-curve
        // drawdown (which will be 0) and CAGR (also 0). Sharpe/Sortino
        // are undefined → stay at 0.0.
        let (dd_abs, dd_pct) = drawdown(&report.equity_curve, report.initial_equity);
        m.max_drawdown_abs = dd_abs;
        m.max_drawdown_pct = dd_pct;
        m.cagr = cagr(report, annualisation_factor);
        return m;
    }

    let (wins, losses, gross_profit, gross_loss, avg_w, avg_l, bars_held_sum) =
        per_trade_summary(trades);
    m.wins = wins;
    m.losses = losses;
    m.win_rate = wins as f64 / trades.len() as f64 * 100.0;
    m.gross_profit = gross_profit;
    m.gross_loss = gross_loss;
    m.net_pnl = trades.iter().map(|t| t.net_pnl).sum();
    m.avg_win = avg_w;
    m.avg_loss = avg_l;
    m.profit_factor = if gross_loss.abs() > 0.0 {
        gross_profit / gross_loss.abs()
    } else if gross_profit > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };
    m.expectancy = m.net_pnl / trades.len() as f64;
    m.avg_bars_held = bars_held_sum / trades.len() as f64;
    let (dd_abs, dd_pct) = drawdown(&report.equity_curve, report.initial_equity);
    m.max_drawdown_abs = dd_abs;
    m.max_drawdown_pct = dd_pct;

    let (sharpe, sortino) = sharpe_sortino(&report.equity_curve, annualisation_factor);
    m.sharpe = sharpe;
    m.sortino = sortino;
    m.cagr = cagr(report, annualisation_factor);

    // Exposure: fraction of equity-curve bars during which a position
    // was open. We don't carry that flag here, so approximate via the
    // per-trade bar count divided by processed bars.
    let exposed_bars: u32 = trades.iter().map(|t| t.bars_held.max(1)).sum();
    let total_bars = report.equity_curve.len().max(1) as f64;
    m.exposure_pct = (exposed_bars as f64 / total_bars * 100.0).clamp(0.0, 100.0);

    m
}

fn per_trade_summary(trades: &[SimTrade]) -> (usize, usize, f64, f64, f64, f64, f64) {
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut gp = 0.0;
    let mut gl = 0.0;
    let mut bars_held_sum = 0.0;
    for t in trades {
        if t.net_pnl > 0.0 {
            wins += 1;
            gp += t.net_pnl;
        } else if t.net_pnl < 0.0 {
            losses += 1;
            gl += t.net_pnl;
        }
        bars_held_sum += t.bars_held as f64;
    }
    let avg_w = if wins > 0 { gp / wins as f64 } else { 0.0 };
    let avg_l = if losses > 0 { gl / losses as f64 } else { 0.0 };
    (wins, losses, gp, gl, avg_w, avg_l, bars_held_sum)
}

/// Max drawdown from peak equity. Returns (absolute, percent of peak).
fn drawdown(curve: &[EquityPoint], initial: f64) -> (f64, f64) {
    if curve.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = initial.max(curve[0].equity);
    let mut worst_abs = 0.0;
    let mut worst_pct = 0.0;
    for p in curve {
        if p.equity > peak {
            peak = p.equity;
        }
        let dd_abs = peak - p.equity;
        if dd_abs > worst_abs {
            worst_abs = dd_abs;
            worst_pct = if peak > 0.0 { dd_abs / peak * 100.0 } else { 0.0 };
        }
    }
    (worst_abs, worst_pct)
}

/// Sharpe (mean / std × √annualisation) + Sortino (mean / downside-std).
/// Both computed on per-bar returns of the equity curve. Returns 0.0
/// when fewer than 2 samples or zero std.
fn sharpe_sortino(curve: &[EquityPoint], annualisation_factor: f64) -> (f64, f64) {
    if curve.len() < 2 {
        return (0.0, 0.0);
    }
    let mut rets: Vec<f64> = Vec::with_capacity(curve.len() - 1);
    for w in curve.windows(2) {
        let prev = w[0].equity;
        let now = w[1].equity;
        if prev > 0.0 {
            rets.push((now - prev) / prev);
        }
    }
    if rets.is_empty() {
        return (0.0, 0.0);
    }
    let n = rets.len() as f64;
    let mean = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let std = var.sqrt();
    let sharpe = if std > 0.0 {
        mean / std * annualisation_factor.sqrt()
    } else {
        0.0
    };
    // Sortino: downside deviation only.
    let downside: Vec<f64> = rets.iter().filter(|r| **r < 0.0).copied().collect();
    let sortino = if !downside.is_empty() {
        let dvar = downside.iter().map(|r| r.powi(2)).sum::<f64>() / downside.len() as f64;
        let dstd = dvar.sqrt();
        if dstd > 0.0 {
            mean / dstd * annualisation_factor.sqrt()
        } else {
            0.0
        }
    } else {
        0.0
    };
    (sharpe, sortino)
}

/// Compound annual growth rate. `annualisation_factor` is bars/year.
fn cagr(report: &SimReport, annualisation_factor: f64) -> f64 {
    if report.initial_equity <= 0.0 || report.final_equity <= 0.0 {
        return 0.0;
    }
    let bars = report.equity_curve.len().max(1) as f64;
    let years = bars / annualisation_factor.max(1.0);
    if years <= 0.0 {
        return 0.0;
    }
    let ratio = report.final_equity / report.initial_equity;
    (ratio.powf(1.0 / years) - 1.0) * 100.0
}

/// Convenience: derive an annualisation factor from a timeframe string.
/// Assumes RTH-only equities (252 days × in-session bars).
pub fn annualisation_for_tf(tf: &str) -> f64 {
    let bars_per_rth = match tf {
        "1m" => 390.0,
        "5m" => 78.0,
        "10m" => 39.0,
        "15m" => 26.0,
        "30m" => 13.0,
        "1h" => 6.5,
        "4h" => 1.5,
        "1d" | "1D" => 1.0,
        _ => 78.0,
    };
    bars_per_rth * 252.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smarttrend::strategies::StrategyId;
    use crate::state::Direction;

    fn trade(net: f64, bars: u32) -> SimTrade {
        SimTrade {
            strategy: StrategyId::OpeningRangeBreakout,
            symbol: "T".into(),
            side: Direction::Long,
            entry_time: 0,
            exit_time: 0,
            entry_price: 100.0,
            exit_price: 100.0 + net / 10.0,
            qty: 10.0,
            gross_pnl: net,
            fees: 0.0,
            net_pnl: net,
            exit_reason: "x".into(),
            bars_held: bars,
        }
    }

    fn report_with(trades: Vec<SimTrade>, eq: Vec<f64>) -> SimReport {
        SimReport {
            strategy: StrategyId::OpeningRangeBreakout,
            symbol: "T".into(),
            timeframe: "5m".into(),
            trades,
            equity_curve: eq
                .into_iter()
                .enumerate()
                .map(|(i, e)| EquityPoint {
                    time: i as i64 * 300,
                    equity: e,
                })
                .collect(),
            initial_equity: 10_000.0,
            final_equity: 10_000.0,
            bars_processed: 100,
        }
    }

    #[test]
    fn metrics_empty_trades_drawdown_only() {
        let r = report_with(vec![], vec![10_000.0; 10]);
        let m = compute(&r, 252.0);
        assert_eq!(m.trades, 0);
        assert_eq!(m.win_rate, 0.0);
        assert_eq!(m.max_drawdown_pct, 0.0);
    }

    #[test]
    fn metrics_win_rate_and_profit_factor() {
        let trades = vec![trade(100.0, 2), trade(-50.0, 3), trade(60.0, 1)];
        let r = report_with(trades, vec![10_000.0, 10_100.0, 10_050.0, 10_110.0]);
        let m = compute(&r, 252.0);
        assert_eq!(m.trades, 3);
        assert_eq!(m.wins, 2);
        assert_eq!(m.losses, 1);
        assert!((m.win_rate - 66.6666).abs() < 0.01);
        // gross profit 160, gross loss 50 → PF = 3.2
        assert!((m.profit_factor - 3.2).abs() < 1e-6);
        assert!((m.net_pnl - 110.0).abs() < 1e-9);
    }

    #[test]
    fn drawdown_simple() {
        let r = report_with(vec![], vec![10_000.0, 11_000.0, 10_500.0, 9_500.0, 10_500.0]);
        let m = compute(&r, 252.0);
        // Peak = 11k, trough = 9.5k → DD = 1500 / 11000 ≈ 13.636 %
        assert!((m.max_drawdown_pct - 13.636363).abs() < 0.01);
        assert!((m.max_drawdown_abs - 1500.0).abs() < 1e-6);
    }

    #[test]
    fn sharpe_zero_when_no_variance() {
        let r = report_with(vec![], vec![10_000.0; 10]);
        let m = compute(&r, 252.0);
        assert_eq!(m.sharpe, 0.0);
        assert_eq!(m.sortino, 0.0);
    }

    #[test]
    fn cagr_positive_when_equity_grows() {
        // 100 bars at 252 bars/yr = ~0.397 years. Final/Initial = 1.1.
        // CAGR = 1.1^(1/0.397) - 1 ≈ 27.4%
        let r = SimReport {
            strategy: StrategyId::OpeningRangeBreakout,
            symbol: "T".into(),
            timeframe: "1d".into(),
            trades: vec![],
            equity_curve: (0..100).map(|i| EquityPoint { time: i, equity: 10_000.0 + i as f64 * 10.0 }).collect(),
            initial_equity: 10_000.0,
            final_equity: 10_990.0,
            bars_processed: 100,
        };
        let m = compute(&r, 252.0);
        assert!(m.cagr > 0.0, "got {}", m.cagr);
    }

    #[test]
    fn annualisation_factor_known_values() {
        assert_eq!(annualisation_for_tf("1d"), 252.0);
        assert_eq!(annualisation_for_tf("5m"), 78.0 * 252.0);
    }
}
