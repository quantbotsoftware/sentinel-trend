//! OMEGA-Frontier Phase II & III (Bucla Micro & Dynamic Capital Reallocation) — Sandbox validation logic.
//!
//! Validates candidate parameter mutations by running an A/B portfolio backtest
//! on the last 30 days of 5-minute candles. Also runs 7-day regime-change backtests
//! to reallocate capital dynamically.

use std::collections::HashMap;
use std::sync::Arc;
use titan_types::Candle;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::backtest::engine::{simulate, EquityPoint, SimParams, SimReport};
use crate::backtest::fills::FillModel;
use crate::backtest::metrics::{annualisation_for_tf, compute};
use crate::smarttrend::strategies::{by_id, StrategyId};
use crate::smarttrend::strategy_evolver::ParamOverrides;

/// Runs A/B simulations comparing current overrides vs candidate overrides on the last 30 days.
/// Returns true if candidate parameters improve the Profit Factor by >= 10%.
pub fn validate_and_apply_mutation(
    strategy_id: StrategyId,
    candidate_params: &HashMap<String, f64>,
    candles_90d: &HashMap<String, Vec<Candle>>,
    overrides: &ParamOverrides,
) -> bool {
    // 1. Resolve specs to build the current active parameters map
    let specs = crate::smarttrend::genetic_optimizer::default_specs_for(strategy_id);
    if specs.is_empty() {
        tracing::warn!(
            component = "sandbox",
            strategy = ?strategy_id,
            "No parameter specs found for strategy"
        );
        return false;
    }

    let mut current_params = HashMap::new();
    for spec in &specs {
        let val = overrides.get(strategy_id, &spec.name)
            .unwrap_or_else(|| (spec.min + spec.max) / 2.0);
        current_params.insert(spec.name.clone(), val);
    }

    // 2. Extract recent 30-day candles
    let cutoff = chrono::Utc::now().timestamp() - 30 * 24 * 3600;
    let mut candles_30d = HashMap::new();
    for (symbol, bars) in candles_90d {
        let recent_bars: Vec<Candle> = bars.iter().filter(|c| c.time >= cutoff).cloned().collect();
        if !recent_bars.is_empty() {
            candles_30d.insert(symbol.clone(), recent_bars);
        }
    }

    if candles_30d.is_empty() {
        tracing::warn!(
            component = "sandbox",
            strategy = ?strategy_id,
            "No recent candle data available for sandbox validation"
        );
        return false;
    }

    let strategy = match by_id(strategy_id) {
        Some(s) => s,
        None => return false,
    };

    let initial_equity = 10_000.0;

    // 3. A-simulation: current parameters
    let mut current_trades = Vec::new();
    for (symbol, bars) in &candles_30d {
        let params = SimParams {
            symbol: symbol.clone(),
            timeframe: "5m",
            initial_equity,
            fill_model: FillModel {
                slippage_bps: 5.0,
                commission: 0.0,
                per_share_fee: 0.0,
            },
            warmup_bars: 30,
            spread_bps: 5.0,
            overrides: current_params.clone(),
        };
        let report = simulate(strategy, bars, &params);
        current_trades.extend(report.trades);
    }

    // 4. B-simulation: candidate parameters
    let mut candidate_trades = Vec::new();
    for (symbol, bars) in &candles_30d {
        let params = SimParams {
            symbol: symbol.clone(),
            timeframe: "5m",
            initial_equity,
            fill_model: FillModel {
                slippage_bps: 5.0,
                commission: 0.0,
                per_share_fee: 0.0,
            },
            warmup_bars: 30,
            spread_bps: 5.0,
            overrides: candidate_params.clone(),
        };
        let report = simulate(strategy, bars, &params);
        candidate_trades.extend(report.trades);
    }

    // Sort exit times to evaluate combined portfolios
    current_trades.sort_by_key(|t| t.exit_time);
    candidate_trades.sort_by_key(|t| t.exit_time);

    // Build SimReports
    let mut current_curve = Vec::new();
    let mut current_cash = initial_equity;
    if let Some(first) = current_trades.first() {
        current_curve.push(EquityPoint {
            time: first.entry_time,
            equity: current_cash,
        });
    }
    for t in &current_trades {
        current_cash += t.net_pnl;
        current_curve.push(EquityPoint {
            time: t.exit_time,
            equity: current_cash,
        });
    }
    let current_report = SimReport {
        strategy: strategy_id,
        symbol: "PORTFOLIO".into(),
        timeframe: "5m".into(),
        trades: current_trades,
        equity_curve: current_curve,
        initial_equity,
        final_equity: current_cash,
        bars_processed: 0,
    };

    let mut candidate_curve = Vec::new();
    let mut candidate_cash = initial_equity;
    if let Some(first) = candidate_trades.first() {
        candidate_curve.push(EquityPoint {
            time: first.entry_time,
            equity: candidate_cash,
        });
    }
    for t in &candidate_trades {
        candidate_cash += t.net_pnl;
        candidate_curve.push(EquityPoint {
            time: t.exit_time,
            equity: candidate_cash,
        });
    }
    let candidate_report = SimReport {
        strategy: strategy_id,
        symbol: "PORTFOLIO".into(),
        timeframe: "5m".into(),
        trades: candidate_trades,
        equity_curve: candidate_curve,
        initial_equity,
        final_equity: candidate_cash,
        bars_processed: 0,
    };

    let current_metrics = compute(&current_report, annualisation_for_tf("5m"));
    let candidate_metrics = compute(&candidate_report, annualisation_for_tf("5m"));

    let current_pf = current_metrics.profit_factor;
    let candidate_pf = candidate_metrics.profit_factor;

    // Apply overrides ONLY if profit factor improves by at least 10%
    let is_better = if candidate_metrics.trades == 0 {
        false
    } else if current_metrics.trades == 0 {
        candidate_pf > 0.0
    } else {
        candidate_pf >= current_pf * 1.10
    };

    tracing::info!(
        component = "sandbox",
        strategy = ?strategy_id,
        current_pf = current_pf,
        current_trades = current_metrics.trades,
        candidate_pf = candidate_pf,
        candidate_trades = candidate_metrics.trades,
        is_better = is_better,
        "Sandbox validation completed"
    );

    if is_better {
        overrides.set(strategy_id, candidate_params.clone());
        true
    } else {
        false
    }
}

/// Runs a quick 7-day backtest on all strategies and reallocates initial/current sub-wallet capital.
pub async fn run_regime_reallocation(
    state: Arc<crate::state::TitanState>,
    _market_data: Arc<crate::market::market_state::MarketDataState>,
    app: tauri::AppHandle,
    new_regime: crate::smarttrend::strategies::MarketRegime,
) -> Result<(), String> {
    // 1. Get symbols
    let mut symbols: Vec<String> = {
        let market = state.market.lock().unwrap();
        market.auto_universe.active_set().into_iter().collect()
    };
    if symbols.is_empty() {
        let cfg = state.config.lock().unwrap();
        symbols = cfg.ui.favorites.clone();
    }
    if symbols.is_empty() {
        symbols = vec!["AAPL".to_string(), "MSFT".to_string(), "NVDA".to_string()];
    }

    // 2. Fetch 7-day candles
    let (key, secret) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };
    let end = chrono::Utc::now();
    let start = end - chrono::Duration::days(7);
    let cache_dir = Some(crate::paths::data_dir().join("backtest_cache"));

    let (candles_7d, failed) = crate::backtest::runner::fetch_universe(
        &symbols,
        "5m",
        start,
        end,
        &state.http_client,
        &key,
        &secret,
        cache_dir.as_deref(),
    )
    .await;

    if !failed.is_empty() {
        tracing::warn!(
            component = "reallocation",
            failed_symbols = ?failed,
            "Failed to fetch 7-day candles for some symbols during regime change reallocation"
        );
    }

    if !candles_7d.is_empty() {
        if let Some(dp) = state.dplus() {
            let mut t = dp.tournament.lock().map_err(|e| e.to_string())?;
            reallocate_capital_on_regime(&mut t, &candles_7d, &dp.overrides, new_regime);
            let _ = t.save(&crate::smarttrend::tournament::default_base_dir());
            use tauri::Manager;
            let _ = app.emit_all("tournament-update", serde_json::json!({"reason": "regime_reallocation"}));
        }
    }

    Ok(())
}

/// Allocates active tournament capital proportionally to recent 7-day performance.
/// Drains any active strategy suffering major losses (net PnL < -5% or Profit Factor < 0.7).
pub fn reallocate_capital_on_regime(
    t: &mut crate::smarttrend::tournament::Tournament,
    candles_7d: &HashMap<String, Vec<Candle>>,
    overrides: &ParamOverrides,
    _new_regime: crate::smarttrend::strategies::MarketRegime,
) {
    let now = chrono::Utc::now().timestamp();
    let mut active_strategies = Vec::new();
    let mut total_active_capital = Decimal::ZERO;

    for id in StrategyId::ALL {
        if let Some(s) = t.stats.get(id) {
            if s.status == crate::smarttrend::tournament::StrategyStatus::Active {
                active_strategies.push(*id);
                total_active_capital += s.initial_sub_wallet;
            }
        }
    }

    if active_strategies.is_empty() || total_active_capital <= Decimal::ZERO {
        return;
    }

    let mut performance_scores = HashMap::new();
    let mut strategies_to_drain = Vec::new();
    let initial_equity = 10_000.0;

    for &sid in &active_strategies {
        let strategy_struct = match by_id(sid) {
            Some(s) => s,
            None => continue,
        };

        let specs = crate::smarttrend::genetic_optimizer::default_specs_for(sid);
        let mut params = HashMap::new();
        for spec in &specs {
            let val = overrides.get(sid, &spec.name)
                .unwrap_or_else(|| (spec.min + spec.max) / 2.0);
            params.insert(spec.name.clone(), val);
        }

        let mut trades = Vec::new();
        for (symbol, bars) in candles_7d {
            let sim_params = SimParams {
                symbol: symbol.clone(),
                timeframe: "5m",
                initial_equity,
                fill_model: FillModel {
                    slippage_bps: 5.0,
                    commission: 0.0,
                    per_share_fee: 0.0,
                },
                warmup_bars: 30,
                spread_bps: 5.0,
                overrides: params.clone(),
            };
            let report = simulate(strategy_struct, bars, &sim_params);
            trades.extend(report.trades);
        }

        trades.sort_by_key(|tr| tr.exit_time);
        let mut curve = Vec::new();
        let mut cash = initial_equity;
        if let Some(first) = trades.first() {
            curve.push(EquityPoint {
                time: first.entry_time,
                equity: cash,
            });
        }
        for tr in &trades {
            cash += tr.net_pnl;
            curve.push(EquityPoint {
                time: tr.exit_time,
                equity: cash,
            });
        }
        let report = SimReport {
            strategy: sid,
            symbol: "PORTFOLIO".into(),
            timeframe: "5m".into(),
            trades,
            equity_curve: curve,
            initial_equity,
            final_equity: cash,
            bars_processed: 0,
        };
        let metrics = compute(&report, annualisation_for_tf("5m"));

        let pnl_pct = (metrics.final_equity - metrics.initial_equity) / metrics.initial_equity;
        // Major loss threshold: net PnL < -5% or Profit Factor < 0.7
        if metrics.trades > 0 && (pnl_pct < -0.05 || metrics.profit_factor < 0.7) {
            strategies_to_drain.push(sid);
            performance_scores.insert(sid, 0.0);
        } else {
            let score = (metrics.final_equity - metrics.initial_equity).max(0.0);
            performance_scores.insert(sid, score);
        }
    }

    for sid in strategies_to_drain {
        tracing::warn!(
            component = "reallocation",
            strategy = ?sid,
            "Strategy suffered major losses during recent 7-day backtest. Transitioning to Draining."
        );
        t.drain(sid, now);
        active_strategies.retain(|&x| x != sid);
    }

    if active_strategies.is_empty() {
        return;
    }

    let num_active = active_strategies.len();
    let min_allocation_pct = dec!(0.05);
    let min_allocated_total = total_active_capital * min_allocation_pct * Decimal::from(num_active);
    let pool_to_distribute = total_active_capital - min_allocated_total;

    let sum_scores: f64 = performance_scores.values().sum();
    let mut shares = HashMap::new();

    if sum_scores > 0.0 {
        for &sid in &active_strategies {
            let score = performance_scores.get(&sid).copied().unwrap_or(0.0);
            let share = score / sum_scores;
            shares.insert(sid, Decimal::from_f64_retain(share).unwrap_or(Decimal::ZERO));
        }
    } else {
        let equal_share = Decimal::ONE / Decimal::from(num_active);
        for &sid in &active_strategies {
            shares.insert(sid, equal_share);
        }
    }

    for &sid in &active_strategies {
        let min_alloc = total_active_capital * min_allocation_pct;
        let bonus_alloc = pool_to_distribute * shares.get(&sid).copied().unwrap_or(Decimal::ZERO);
        let final_alloc = (min_alloc + bonus_alloc).round_dp(4);

        if let Some(s) = t.stats.get_mut(&sid) {
            tracing::info!(
                component = "reallocation",
                strategy = ?sid,
                old_allocation = ?s.initial_sub_wallet,
                new_allocation = ?final_alloc,
                "Reallocated initial sub-wallet capital based on 7-day performance in new regime"
            );
            let diff = final_alloc - s.initial_sub_wallet;
            s.initial_sub_wallet = final_alloc;
            s.current_sub_wallet += diff;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_candles_validation_fails() {
        let overrides = ParamOverrides::new();
        let candidate_params = HashMap::new();
        let candles_90d = HashMap::new();
        let ok = validate_and_apply_mutation(
            StrategyId::GapFade,
            &candidate_params,
            &candles_90d,
            &overrides,
        );
        assert!(!ok);
    }

    #[test]
    fn test_reallocate_capital_on_regime_no_active_noop() {
        let mut t = crate::smarttrend::tournament::Tournament::new_default(dec!(100000));
        for id in StrategyId::ALL {
            t.kill(*id);
        }
        let overrides = ParamOverrides::new();
        let candles_7d = HashMap::new();
        reallocate_capital_on_regime(
            &mut t,
            &candles_7d,
            &overrides,
            crate::smarttrend::strategies::MarketRegime::Calm,
        );
        assert_eq!(t.free_pool, dec!(100000));
    }
}
