//! Parallel orchestration: many strategies × many symbols → many
//! `SimReport`s in parallel via `rayon::par_iter`. The async candle
//! fetch (rate-limited Alpaca HTTP) is done sequentially up front
//! with a small inter-request delay; the CPU-bound simulations run
//! in parallel after the data is in memory.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use titan_types::Candle;

use crate::commands::market_data::fetch_candles_range_internal;
use crate::error::TitanError;
use crate::smarttrend::strategies::{by_id, StrategyId};

use super::engine::{simulate, SimParams, SimReport};
use super::fills::FillModel;
use super::metrics::{annualisation_for_tf, compute, PerfMetrics};

/// What the operator asks for. One request describes a whole grid.
#[derive(Debug, Clone, Deserialize)]
pub struct BacktestRequest {
    pub symbols: Vec<String>,
    pub strategies: Vec<StrategyId>,
    /// `"1m"` / `"5m"` / `"15m"` / `"1h"` / `"1d"`, etc.
    pub timeframe: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Starting equity per (strategy, symbol) bucket.
    #[serde(default = "default_initial_equity")]
    pub initial_equity: f64,
    /// Slippage in basis points; default 5.
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: f64,
    /// Per-trade commission (USD); default 0 (Alpaca).
    #[serde(default)]
    pub commission: f64,
    /// Constant spread assumption (basis points).
    #[serde(default = "default_spread_bps")]
    pub spread_bps: f64,
    /// Disk-cache toggle. When `true`, candle fetches read/write a
    /// gzip JSON file in `data_dir/backtest_cache/`.
    #[serde(default = "default_use_cache")]
    pub use_cache: bool,
}

fn default_initial_equity() -> f64 {
    10_000.0
}
fn default_slippage_bps() -> f64 {
    5.0
}
fn default_spread_bps() -> f64 {
    5.0
}
fn default_use_cache() -> bool {
    true
}

/// One row of the report: (strategy, symbol) → SimReport + metrics.
#[derive(Debug, Clone, Serialize)]
pub struct PerSymbolRow {
    pub strategy: StrategyId,
    pub symbol: String,
    pub metrics: PerfMetrics,
    pub equity_curve: Vec<super::engine::EquityPoint>,
    pub trades: Vec<super::engine::SimTrade>,
}

/// Aggregate view over all symbols for a single strategy.
#[derive(Debug, Clone, Serialize)]
pub struct PerStrategySummary {
    pub strategy: StrategyId,
    pub metrics: PerfMetrics,
    pub symbols_traded: usize,
    pub equity_curve: Vec<super::engine::EquityPoint>,
}

/// Top-level report returned to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct StrategyBacktestReport {
    pub timeframe: String,
    pub start: i64,
    pub end: i64,
    pub initial_equity: f64,
    pub symbols_fetched: usize,
    pub symbols_failed: Vec<String>,
    pub per_symbol: Vec<PerSymbolRow>,
    pub per_strategy: Vec<PerStrategySummary>,
    pub disclaimer: String,
    /// Total wall-clock time the simulator portion took (ms). Network
    /// time is excluded so the operator can see the speedup from
    /// rayon distinctly.
    pub simulate_ms: u64,
}

/// Top-level entry point. Network I/O is awaited; simulations run on
/// the rayon pool after candle data is collected.
pub async fn run(
    request: &BacktestRequest,
    http_client: &reqwest::Client,
    alpaca_key: &str,
    alpaca_secret: &str,
    cache_dir: Option<PathBuf>,
) -> Result<StrategyBacktestReport, TitanError> {
    if request.symbols.is_empty() {
        return Err(TitanError::Validation("No symbols supplied".into()));
    }
    if request.strategies.is_empty() {
        return Err(TitanError::Validation("No strategies supplied".into()));
    }
    if request.start >= request.end {
        return Err(TitanError::Validation("start must be before end".into()));
    }

    let cache_dir = cache_dir.filter(|_| request.use_cache);

    // ─── Fetch candles (network bound, sequential to avoid bursting Alpaca) ───
    let (bars_by_symbol, symbols_failed) = fetch_universe(
        &request.symbols,
        &request.timeframe,
        request.start,
        request.end,
        http_client,
        alpaca_key,
        alpaca_secret,
        cache_dir.as_deref(),
    )
    .await;

    if bars_by_symbol.is_empty() {
        return Err(TitanError::MarketData(
            "No candle data fetched for any symbol".into(),
        ));
    }

    // ─── Simulate (CPU-bound, parallel) ──────────────────────────
    let tf_static = static_timeframe(&request.timeframe);
    let fill_model = FillModel {
        slippage_bps: request.slippage_bps,
        commission: request.commission,
        per_share_fee: 0.0,
    };
    let initial_equity = request.initial_equity;
    let spread_bps = request.spread_bps;

    // Build a flat list of (strategy, symbol, bars-ref) tasks. Owned
    // by the closure passed to rayon — `bars_by_symbol` is borrowed
    // immutably and survives the parallel section.
    let tasks: Vec<(StrategyId, String)> = request
        .strategies
        .iter()
        .flat_map(|sid| {
            bars_by_symbol
                .keys()
                .map(move |s| (*sid, s.clone()))
                .collect::<Vec<_>>()
        })
        .collect();

    let started = std::time::Instant::now();
    let per_symbol: Vec<PerSymbolRow> = tasks
        .par_iter()
        .filter_map(|(sid, sym)| {
            let strategy = by_id(*sid)?;
            let bars = bars_by_symbol.get(sym)?;
            let params = SimParams {
                symbol: sym.clone(),
                timeframe: tf_static,
                initial_equity,
                fill_model,
                warmup_bars: 30,
                spread_bps,
                overrides: std::collections::HashMap::new(),
            };
            let report = simulate(strategy, bars, &params);
            let metrics = compute(&report, annualisation_for_tf(tf_static));
            Some(PerSymbolRow {
                strategy: *sid,
                symbol: sym.clone(),
                metrics,
                equity_curve: report.equity_curve,
                trades: report.trades,
            })
        })
        .collect();
    let simulate_ms = started.elapsed().as_millis() as u64;

    let per_strategy = aggregate_by_strategy(&per_symbol, initial_equity, tf_static);

    Ok(StrategyBacktestReport {
        timeframe: request.timeframe.clone(),
        start: request.start.timestamp(),
        end: request.end.timestamp(),
        initial_equity,
        symbols_fetched: bars_by_symbol.len(),
        symbols_failed,
        per_symbol,
        per_strategy,
        disclaimer: "Backtest uses simulated fills with configurable slippage and \
                     commission. Realistic for liquid SP500 names; widen slippage \
                     for small-caps. Halt-resume events are absent from historical \
                     bars — strategies that depend on them will under-trade."
            .to_string(),
        simulate_ms,
    })
}

/// Build aggregate metrics per-strategy by chronologically merging
/// trades across symbols. Each strategy's "equity curve" here is
/// `initial_equity + cumulative_pnl` along the trade close timeline.
/// This is intentionally simpler than per-bar mark-to-market across
/// symbols (which would need a portfolio simulator); the headline
/// stats (PF, win-rate, expectancy, DD over closed PnL) remain
/// statistically meaningful.
fn aggregate_by_strategy(
    rows: &[PerSymbolRow],
    initial_equity: f64,
    tf: &str,
) -> Vec<PerStrategySummary> {
    let mut buckets: HashMap<StrategyId, Vec<&PerSymbolRow>> = HashMap::new();
    for r in rows {
        buckets.entry(r.strategy).or_default().push(r);
    }

    let mut out: Vec<PerStrategySummary> = Vec::with_capacity(buckets.len());
    for (sid, group) in buckets {
        let mut all_trades: Vec<super::engine::SimTrade> = group
            .iter()
            .flat_map(|r| r.trades.iter().cloned())
            .collect();
        all_trades.sort_by_key(|t| t.exit_time);

        let mut cash = initial_equity;
        let mut curve: Vec<super::engine::EquityPoint> = Vec::with_capacity(all_trades.len() + 1);
        if let Some(first) = all_trades.first() {
            curve.push(super::engine::EquityPoint {
                time: first.entry_time,
                equity: cash,
            });
        }
        for t in &all_trades {
            cash += t.net_pnl;
            curve.push(super::engine::EquityPoint {
                time: t.exit_time,
                equity: cash,
            });
        }

        let symbols_traded = group.iter().filter(|r| !r.trades.is_empty()).count();
        let synthetic_report = SimReport {
            strategy: sid,
            symbol: "PORTFOLIO".into(),
            timeframe: tf.to_string(),
            trades: all_trades,
            equity_curve: curve.clone(),
            initial_equity,
            final_equity: cash,
            bars_processed: curve.len(),
        };
        let metrics = compute(&synthetic_report, annualisation_for_tf(tf));
        out.push(PerStrategySummary {
            strategy: sid,
            metrics,
            symbols_traded,
            equity_curve: curve,
        });
    }
    // Sort by net P/L descending so the UI table reads leaderboard-style.
    out.sort_by(|a, b| {
        b.metrics
            .net_pnl
            .partial_cmp(&a.metrics.net_pnl)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Fetch candles for a whole universe (sequential, rate-limited, cached)
/// — the same path `run` uses internally, exposed for the headless
/// backtest/sweep CLI so it shares cache files and Alpaca etiquette.
/// Returns `(bars_by_symbol, symbols_failed)`.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_universe(
    symbols: &[String],
    timeframe: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    http_client: &reqwest::Client,
    key: &str,
    secret: &str,
    cache_dir: Option<&std::path::Path>,
) -> (HashMap<String, Vec<Candle>>, Vec<String>) {
    let mut bars_by_symbol: HashMap<String, Vec<Candle>> = HashMap::new();
    let mut failed: Vec<String> = Vec::new();
    for raw_sym in symbols {
        let sym = raw_sym.trim().to_uppercase();
        if sym.is_empty() {
            continue;
        }
        match load_or_fetch_candles(
            &sym, timeframe, start, end, http_client, key, secret, cache_dir,
        )
        .await
        {
            Ok(bars) if !bars.is_empty() => {
                bars_by_symbol.insert(sym, bars);
            }
            Ok(_) => failed.push(sym),
            Err(e) => {
                tracing::warn!(
                    component = "backtest",
                    symbol = %sym,
                    error = %e,
                    "Candle fetch failed; symbol skipped"
                );
                failed.push(sym);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    (bars_by_symbol, failed)
}

/// Map a timeframe string to its `&'static str` canonical form (public
/// for the headless CLI; `SimParams.timeframe` requires `'static`).
pub fn static_timeframe_pub(tf: &str) -> &'static str {
    static_timeframe(tf)
}

#[allow(clippy::too_many_arguments)]
async fn load_or_fetch_candles(
    symbol: &str,
    timeframe: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    http_client: &reqwest::Client,
    key: &str,
    secret: &str,
    cache_dir: Option<&std::path::Path>,
) -> Result<Vec<Candle>, TitanError> {
    if let Some(dir) = cache_dir {
        if let Some(bars) = read_cache(dir, symbol, timeframe, start, end) {
            return Ok(bars);
        }
    }
    let bars = fetch_candles_range_internal(symbol, timeframe, start, end, http_client, key, secret).await?;
    if let Some(dir) = cache_dir {
        if let Err(e) = write_cache(dir, symbol, timeframe, start, end, &bars) {
            tracing::warn!(
                component = "backtest",
                symbol = %symbol,
                error = %e,
                "Cache write failed; continuing without cache"
            );
        }
    }
    Ok(bars)
}

fn cache_path(
    dir: &std::path::Path,
    symbol: &str,
    timeframe: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> PathBuf {
    let fname = format!(
        "{symbol}_{tf}_{s}_{e}.json.gz",
        tf = timeframe,
        s = start.timestamp(),
        e = end.timestamp()
    );
    dir.join(fname)
}

fn read_cache(
    dir: &std::path::Path,
    symbol: &str,
    timeframe: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Option<Vec<Candle>> {
    use std::io::Read;
    let path = cache_path(dir, symbol, timeframe, start, end);
    if !path.exists() {
        return None;
    }
    let file = std::fs::File::open(&path).ok()?;
    let mut gz = flate2::read::GzDecoder::new(file);
    let mut buf = String::new();
    if gz.read_to_string(&mut buf).is_err() {
        return None;
    }
    serde_json::from_str::<Vec<Candle>>(&buf).ok()
}

fn write_cache(
    dir: &std::path::Path,
    symbol: &str,
    timeframe: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    bars: &[Candle],
) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = cache_path(dir, symbol, timeframe, start, end);
    let buf = serde_json::to_string(bars)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let file = std::fs::File::create(&path)?;
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    gz.write_all(buf.as_bytes())?;
    gz.finish()?;
    Ok(())
}

fn static_timeframe(tf: &str) -> &'static str {
    match tf {
        "1m" => "1m",
        "5m" => "5m",
        "10m" => "10m",
        "15m" => "15m",
        "30m" => "30m",
        "1h" => "1h",
        "4h" => "4h",
        "1d" | "1D" => "1d",
        _ => "5m",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::engine::{EquityPoint, SimTrade};
    use crate::state::Direction;

    fn row(sid: StrategyId, sym: &str, trades: Vec<SimTrade>) -> PerSymbolRow {
        PerSymbolRow {
            strategy: sid,
            symbol: sym.into(),
            metrics: Default::default(),
            equity_curve: vec![],
            trades,
        }
    }

    fn trade(net: f64, exit_time: i64) -> SimTrade {
        SimTrade {
            strategy: StrategyId::OpeningRangeBreakout,
            symbol: "X".into(),
            side: Direction::Long,
            entry_time: exit_time - 60,
            exit_time,
            entry_price: 100.0,
            exit_price: 100.0,
            qty: 10.0,
            gross_pnl: net,
            fees: 0.0,
            net_pnl: net,
            exit_reason: "x".into(),
            bars_held: 1,
        }
    }

    #[test]
    fn aggregate_groups_by_strategy_and_orders_by_pnl() {
        let rows = vec![
            row(
                StrategyId::OpeningRangeBreakout,
                "AAPL",
                vec![trade(50.0, 100), trade(-20.0, 200)],
            ),
            row(
                StrategyId::OpeningRangeBreakout,
                "MSFT",
                vec![trade(10.0, 150)],
            ),
            row(StrategyId::GapFade, "TSLA", vec![trade(-5.0, 120)]),
        ];
        let agg = aggregate_by_strategy(&rows, 10_000.0, "5m");
        assert_eq!(agg.len(), 2);
        // ORB has higher net (50 - 20 + 10 = 40) than GapFade (-5).
        assert_eq!(agg[0].strategy, StrategyId::OpeningRangeBreakout);
        assert!((agg[0].metrics.net_pnl - 40.0).abs() < 1e-9);
        assert_eq!(agg[0].symbols_traded, 2);
        assert_eq!(agg[1].strategy, StrategyId::GapFade);
    }

    #[test]
    fn equity_curve_starts_at_initial_and_steps_with_each_trade() {
        let rows = vec![row(
            StrategyId::OpeningRangeBreakout,
            "AAPL",
            vec![trade(100.0, 200), trade(-50.0, 400)],
        )];
        let agg = aggregate_by_strategy(&rows, 10_000.0, "5m");
        let curve = &agg[0].equity_curve;
        assert_eq!(curve.len(), 3);
        assert!((curve[0].equity - 10_000.0).abs() < 1e-9);
        assert!((curve[1].equity - 10_100.0).abs() < 1e-9);
        assert!((curve[2].equity - 10_050.0).abs() < 1e-9);
    }

    #[test]
    fn equity_points_are_unused_warning_silenced() {
        let _p = EquityPoint { time: 0, equity: 0.0 };
    }
}
