#![cfg_attr(test, allow(clippy::unwrap_used))]

use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ═══════════════════════════════════════════════════════════════════
//  CIRCUIT BREAKER — 3-Level Protection System
// ═══════════════════════════════════════════════════════════════════
//
//  All thresholds are POSITIVE percentages (e.g. dec!(3.0) = 3% loss).
//  Level 1 (CAUTION):  3% daily loss  → halve position sizes
//  Level 2 (HALT):     5% daily loss  → block new entries
//  Level 3 (PANIC):    8% daily loss OR max drawdown ≥ panic_pct → liquidate
//
//  All thresholds use rust_decimal for exact comparison.
//  State flags are AtomicBool for lock-free reads on hot path.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BreakerLevel {
    Normal,
    Caution,
    Halt,
    Panic,
}

impl std::fmt::Display for BreakerLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BreakerLevel::Normal => write!(f, "NORMAL"),
            BreakerLevel::Caution => write!(f, "CAUTION"),
            BreakerLevel::Halt => write!(f, "HALT"),
            BreakerLevel::Panic => write!(f, "PANIC"),
        }
    }
}

pub struct CircuitBreaker {
    pub is_armed: AtomicBool,
    pub kill_switch: AtomicBool,
    pub entries_blocked: AtomicBool,
    pub size_reduction_pct: AtomicU64,
    level: std::sync::Mutex<BreakerLevel>,

    caution_pct: std::sync::Mutex<Decimal>,
    halt_pct: std::sync::Mutex<Decimal>,
    panic_pct: std::sync::Mutex<Decimal>,
}

impl CircuitBreaker {
    pub fn new(caution_pct: Decimal, halt_pct: Decimal, panic_pct: Decimal) -> Self {
        Self {
            is_armed: AtomicBool::new(true),
            kill_switch: AtomicBool::new(false),
            entries_blocked: AtomicBool::new(false),
            size_reduction_pct: AtomicU64::new(100),
            level: std::sync::Mutex::new(BreakerLevel::Normal),
            caution_pct: std::sync::Mutex::new(caution_pct),
            halt_pct: std::sync::Mutex::new(halt_pct),
            panic_pct: std::sync::Mutex::new(panic_pct),
        }
    }

    #[cfg(test)]
    pub fn with_defaults() -> Self {
        Self::new(dec!(3.0), dec!(5.0), dec!(8.0))
    }

    pub fn update_thresholds(&self, caution: Decimal, halt: Decimal, panic: Decimal) {
        *self.caution_pct.lock().unwrap_or_else(|e| e.into_inner()) = caution;
        *self.halt_pct.lock().unwrap_or_else(|e| e.into_inner()) = halt;
        *self.panic_pct.lock().unwrap_or_else(|e| e.into_inner()) = panic;
    }

    /// Evaluate breaker level from daily loss % and max drawdown %.
    /// Both inputs are POSITIVE percentages (e.g. 3.5 means 3.5% loss).
    pub fn evaluate(&self, daily_loss_pct: Decimal, max_drawdown_pct: Decimal) -> BreakerLevel {
        let caution = *self.caution_pct.lock().unwrap_or_else(|e| e.into_inner());
        let halt = *self.halt_pct.lock().unwrap_or_else(|e| e.into_inner());
        let panic = *self.panic_pct.lock().unwrap_or_else(|e| e.into_inner());

        let new_level = if max_drawdown_pct >= panic || daily_loss_pct >= panic {
            BreakerLevel::Panic
        } else if daily_loss_pct >= halt {
            BreakerLevel::Halt
        } else if daily_loss_pct >= caution {
            BreakerLevel::Caution
        } else {
            BreakerLevel::Normal
        };

        match new_level {
            BreakerLevel::Panic => {
                self.is_armed.store(false, Ordering::SeqCst);
                self.kill_switch.store(true, Ordering::SeqCst);
                self.entries_blocked.store(true, Ordering::SeqCst);
                self.size_reduction_pct.store(0, Ordering::Relaxed);
            }
            BreakerLevel::Halt => {
                self.entries_blocked.store(true, Ordering::SeqCst);
                self.size_reduction_pct.store(0, Ordering::Relaxed);
            }
            BreakerLevel::Caution => {
                self.entries_blocked.store(false, Ordering::SeqCst);
                self.size_reduction_pct.store(50, Ordering::Relaxed);
            }
            BreakerLevel::Normal => {
                self.entries_blocked.store(false, Ordering::SeqCst);
                self.size_reduction_pct.store(100, Ordering::Relaxed);
            }
        }

        if let Ok(mut lvl) = self.level.lock() {
            *lvl = new_level;
        }
        new_level
    }

    pub fn level(&self) -> BreakerLevel {
        self.level
            .lock()
            .map(|l| *l)
            .unwrap_or(BreakerLevel::Normal)
    }

    pub fn can_enter(&self) -> bool {
        self.is_armed.load(Ordering::SeqCst) && !self.entries_blocked.load(Ordering::SeqCst)
    }

    pub fn size_multiplier(&self) -> Decimal {
        let pct = self.size_reduction_pct.load(Ordering::Relaxed);
        Decimal::from(pct) / dec!(100)
    }

    pub fn manual_reset(&self) {
        self.is_armed.store(true, Ordering::SeqCst);
        self.kill_switch.store(false, Ordering::SeqCst);
        self.entries_blocked.store(false, Ordering::SeqCst);
        self.size_reduction_pct.store(100, Ordering::Relaxed);
        if let Ok(mut lvl) = self.level.lock() {
            *lvl = BreakerLevel::Normal;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  SESSION ANALYTICS — Real-time Performance Tracking
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct SessionAnalytics {
    pub total_trades: u32,
    pub winning_trades: u32,
    pub losing_trades: u32,
    pub total_pnl: Decimal,
    pub daily_pnl: Decimal,
    pub max_drawdown_pct: Decimal,
    pub peak_equity: Decimal,
    pub current_equity: Decimal,
    pub avg_win: Decimal,
    pub avg_loss: Decimal,
    pub profit_factor: Decimal,
    pub win_rate_pct: Decimal,
    pub session_start_equity: Decimal,
    pub last_entry_time: Option<u64>,
    trade_pnls: VecDeque<Decimal>,
    pub longest_win_streak: u32,
    pub longest_lose_streak: u32,
    pub current_streak: i32,
    pub avg_duration_secs: f64,
    pub best_trade_pnl: Decimal,
    pub worst_trade_pnl: Decimal,
    duration_sum_secs: f64,
    duration_count: u32,
    dd_velocity_ring: VecDeque<(u64, f64)>,
    pub dd_velocity: f64,
    running_gross_wins: Decimal,
    running_gross_losses: Decimal,
    sharpe_count: u64,
    sharpe_mean: f64,
    sharpe_m2: f64,
}

const TRADE_PNLS_CAP: usize = 2000;

impl SessionAnalytics {
    pub fn new(starting_equity: Decimal) -> Self {
        Self {
            total_trades: 0,
            winning_trades: 0,
            losing_trades: 0,
            total_pnl: Decimal::ZERO,
            daily_pnl: Decimal::ZERO,
            max_drawdown_pct: Decimal::ZERO,
            peak_equity: starting_equity,
            current_equity: starting_equity,
            avg_win: Decimal::ZERO,
            avg_loss: Decimal::ZERO,
            profit_factor: Decimal::ZERO,
            win_rate_pct: Decimal::ZERO,
            session_start_equity: starting_equity,
            last_entry_time: None,
            trade_pnls: VecDeque::new(),
            longest_win_streak: 0,
            longest_lose_streak: 0,
            current_streak: 0,
            avg_duration_secs: 0.0,
            best_trade_pnl: Decimal::ZERO,
            worst_trade_pnl: Decimal::ZERO,
            duration_sum_secs: 0.0,
            duration_count: 0,
            dd_velocity_ring: VecDeque::with_capacity(64),
            dd_velocity: 0.0,
            running_gross_wins: Decimal::ZERO,
            running_gross_losses: Decimal::ZERO,
            sharpe_count: 0,
            sharpe_mean: 0.0,
            sharpe_m2: 0.0,
        }
    }

    pub fn record_trade(&mut self, pnl: Decimal) {
        self.record_trade_with_duration(pnl, 0);
    }

    pub fn record_trade_with_duration(&mut self, pnl: Decimal, duration_secs: u64) {
        self.total_trades += 1;
        self.total_pnl += pnl;
        self.daily_pnl += pnl;
        self.current_equity += pnl;
        self.trade_pnls.push_back(pnl);
        if self.trade_pnls.len() > TRADE_PNLS_CAP {
            self.trade_pnls.pop_front();
        }

        let val = pnl.to_f64().unwrap_or(0.0);
        self.sharpe_count += 1;
        let delta = val - self.sharpe_mean;
        self.sharpe_mean += delta / self.sharpe_count as f64;
        let delta2 = val - self.sharpe_mean;
        self.sharpe_m2 += delta * delta2;

        if pnl > Decimal::ZERO {
            self.winning_trades += 1;
            self.running_gross_wins += pnl;
            if self.current_streak > 0 {
                self.current_streak += 1;
            } else {
                self.current_streak = 1;
            }
            if self.current_streak as u32 > self.longest_win_streak {
                self.longest_win_streak = self.current_streak as u32;
            }
        } else if pnl < Decimal::ZERO {
            self.losing_trades += 1;
            self.running_gross_losses += pnl.abs();
            if self.current_streak < 0 {
                self.current_streak -= 1;
            } else {
                self.current_streak = -1;
            }
            if (-self.current_streak) as u32 > self.longest_lose_streak {
                self.longest_lose_streak = (-self.current_streak) as u32;
            }
        }

        if pnl > self.best_trade_pnl {
            self.best_trade_pnl = pnl;
        }
        if pnl < self.worst_trade_pnl {
            self.worst_trade_pnl = pnl;
        }

        if duration_secs > 0 {
            self.duration_sum_secs += duration_secs as f64;
            self.duration_count += 1;
            self.avg_duration_secs = self.duration_sum_secs / self.duration_count as f64;
        }

        if self.current_equity > self.peak_equity {
            self.peak_equity = self.current_equity;
        }

        if self.peak_equity > Decimal::ZERO {
            let drawdown = (self.peak_equity - self.current_equity) / self.peak_equity * dec!(100);
            if drawdown > self.max_drawdown_pct {
                self.max_drawdown_pct = drawdown;
            }
        }

        self.recalculate_stats();
    }

    pub fn record_entry_time(&mut self) {
        self.last_entry_time = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
    }

    pub fn cooldown_elapsed(&self, cooldown_secs: u64) -> bool {
        match self.last_entry_time {
            None => true,
            Some(last) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                now.saturating_sub(last) >= cooldown_secs
            }
        }
    }

    pub fn daily_pnl_pct(&self) -> Decimal {
        if self.session_start_equity > Decimal::ZERO {
            self.daily_pnl / self.session_start_equity * dec!(100)
        } else {
            Decimal::ZERO
        }
    }

    pub fn reset_daily(&mut self) {
        self.daily_pnl = Decimal::ZERO;
        self.session_start_equity = self.current_equity;
    }

    pub fn reset_full(&mut self) {
        let equity = self.current_equity;
        *self = Self::new(equity);
    }

    /// Sharpe ratio computed via Welford online algorithm over ALL session trades.
    ///
    /// NOTE: `sharpe_count`/`sharpe_mean`/`sharpe_m2` accumulate over the full session
    /// (not just the capped `trade_pnls` window). After 2000+ trades the ring buffer
    /// holds only the most recent 2000 PnLs for serialization, but Sharpe reflects the
    /// entire session history. This is intentional -- rolling Welford on a sliding window
    /// requires O(n) correction per eviction and the all-history metric is the standard
    /// approach for session-level risk assessment.
    pub fn sharpe_ratio(&self) -> f64 {
        if self.sharpe_count < 2 {
            return 0.0;
        }
        let n = self.sharpe_count as f64;
        let var = self.sharpe_m2 / n;
        let std = var.sqrt();
        if std > 1e-12 {
            self.sharpe_mean / std * n.sqrt()
        } else {
            0.0
        }
    }

    pub fn trade_pnls(&self) -> Vec<Decimal> {
        self.trade_pnls.iter().copied().collect()
    }

    pub fn restore_trade_pnls(&mut self, pnls: Vec<Decimal>) {
        self.running_gross_wins = Decimal::ZERO;
        self.running_gross_losses = Decimal::ZERO;
        self.sharpe_count = 0;
        self.sharpe_mean = 0.0;
        self.sharpe_m2 = 0.0;
        for p in &pnls {
            if *p > Decimal::ZERO {
                self.running_gross_wins += *p;
            } else if *p < Decimal::ZERO {
                self.running_gross_losses += p.abs();
            }
            let val = p.to_f64().unwrap_or(0.0);
            self.sharpe_count += 1;
            let delta = val - self.sharpe_mean;
            self.sharpe_mean += delta / self.sharpe_count as f64;
            let delta2 = val - self.sharpe_mean;
            self.sharpe_m2 += delta * delta2;
        }
        let capped: VecDeque<Decimal> = if pnls.len() > TRADE_PNLS_CAP {
            pnls[pnls.len() - TRADE_PNLS_CAP..].iter().copied().collect()
        } else {
            pnls.into_iter().collect()
        };
        self.trade_pnls = capped;
        self.recalculate_stats();
    }

    /// Snapshot the dd_velocity_ring for state persistence.
    pub fn dd_velocity_ring_snapshot(&self) -> Vec<(u64, f64)> {
        self.dd_velocity_ring.iter().copied().collect()
    }

    /// Restore dd_velocity_ring from a persisted snapshot and recompute velocity.
    pub fn restore_dd_velocity_ring(&mut self, ring: Vec<(u64, f64)>) {
        self.dd_velocity_ring = ring.into_iter().collect();
        let now_secs = self.dd_velocity_ring.back().map(|(t, _)| *t).unwrap_or(0);
        self.dd_velocity = self.compute_dd_velocity(now_secs);
    }

    fn recalculate_stats(&mut self) {
        if self.total_trades == 0 {
            return;
        }

        let total = Decimal::from(self.total_trades);
        self.win_rate_pct = Decimal::from(self.winning_trades) / total * dec!(100);

        if self.winning_trades > 0 {
            self.avg_win = self.running_gross_wins / Decimal::from(self.winning_trades);
        }
        if self.losing_trades > 0 {
            self.avg_loss = self.running_gross_losses / Decimal::from(self.losing_trades);
        }
        if self.running_gross_losses > Decimal::ZERO {
            self.profit_factor = self.running_gross_wins / self.running_gross_losses;
        } else if self.running_gross_wins > Decimal::ZERO {
            self.profit_factor = dec!(999.0);
        }
    }

    pub fn update_equity(&mut self, new_equity: Decimal) {
        self.current_equity = new_equity;
        if new_equity > self.peak_equity {
            self.peak_equity = new_equity;
        }
        if self.peak_equity > Decimal::ZERO {
            let drawdown = (self.peak_equity - self.current_equity) / self.peak_equity * dec!(100);
            if drawdown > self.max_drawdown_pct {
                self.max_drawdown_pct = drawdown;
            }
        }
    }

    /// Record current drawdown percentage with timestamp for velocity tracking.
    /// Call this every autopilot cycle (1-2s) with the current epoch seconds.
    pub fn record_dd_snapshot(&mut self, now_epoch_secs: u64) {
        let dd_pct = if self.peak_equity > Decimal::ZERO {
            ((self.peak_equity - self.current_equity) / self.peak_equity * dec!(100))
                .to_f64()
                .unwrap_or(0.0)
        } else {
            0.0
        };

        self.dd_velocity_ring.push_back((now_epoch_secs, dd_pct));

        const MAX_RING: usize = 120;
        while self.dd_velocity_ring.len() > MAX_RING {
            self.dd_velocity_ring.pop_front();
        }

        self.dd_velocity = self.compute_dd_velocity(now_epoch_secs);
    }

    /// Compute drawdown velocity: change in dd% per minute over the lookback window.
    /// Returns negative values when drawdown is deepening (e.g. -1.2 = losing 1.2%/min).
    fn compute_dd_velocity(&self, now_secs: u64) -> f64 {
        if self.dd_velocity_ring.len() < 2 {
            return 0.0;
        }

        let (oldest_ts, oldest_dd) = self.dd_velocity_ring.front().copied().unwrap_or((now_secs, 0.0));
        let (newest_ts, newest_dd) = self.dd_velocity_ring.back().copied().unwrap_or((now_secs, 0.0));

        let elapsed_mins = (newest_ts.saturating_sub(oldest_ts)) as f64 / 60.0;
        if elapsed_mins < 0.05 {
            return 0.0;
        }

        // dd_pct goes up when losing money, so positive delta = deepening drawdown.
        // We return negative velocity when drawdown deepens (convention: negative = bad).
        let dd_delta = newest_dd - oldest_dd;
        -(dd_delta / elapsed_mins)
    }

    /// Check if drawdown velocity exceeds the threshold, signaling a halt.
    /// `threshold` should be negative (e.g. -1.0 means halt at 1%/min loss velocity).
    /// `window_secs` controls how far back we look in the ring buffer for the velocity calc.
    pub fn check_dd_velocity_halt(&self, threshold: f64, window_secs: u64) -> bool {
        if self.dd_velocity_ring.len() < 2 || threshold >= 0.0 {
            return false;
        }

        let now = self.dd_velocity_ring.back().map(|(t, _)| *t).unwrap_or(0);
        let cutoff = now.saturating_sub(window_secs);

        let window_oldest = self
            .dd_velocity_ring
            .iter()
            .find(|(ts, _)| *ts >= cutoff)
            .copied();

        let window_newest = self.dd_velocity_ring.back().copied();

        match (window_oldest, window_newest) {
            (Some((old_ts, old_dd)), Some((new_ts, new_dd))) => {
                let elapsed_mins = (new_ts.saturating_sub(old_ts)) as f64 / 60.0;
                if elapsed_mins < 0.05 {
                    return false;
                }
                let velocity = -(new_dd - old_dd) / elapsed_mins;
                velocity < threshold
            }
            _ => false,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  POSITION SIZING — Risk-Based (not all-in)
// ═══════════════════════════════════════════════════════════════════
//
//  Formula: qty = (equity * risk_pct) / |entry_price - stop_loss|
//  This risks exactly risk_pct of equity per trade, regardless of
//  the asset's price level.

pub struct SizingParams {
    pub equity: Decimal,
    pub risk_pct: Decimal,
    pub entry_price: Decimal,
    pub stop_loss_price: Decimal,
    pub breaker_multiplier: Decimal,
    pub max_notional_frac: Decimal,
    pub min_stop_frac: Decimal,
}

pub struct SizingResult {
    pub qty: Decimal,
    pub notional: Decimal,
    /// Raw risk_amount = equity * risk_pct / 100 * breaker_multiplier (before notional cap).
    pub risk_amount: Decimal,
    /// Effective risk = qty * stop_distance (after notional cap, if any).
    pub risk_amount_effective: Decimal,
    /// True if notional was capped to max_notional_frac * equity.
    pub capped: bool,
}

pub fn calculate_position_size(params: &SizingParams) -> Option<SizingResult> {
    if params.equity <= Decimal::ZERO || params.entry_price <= Decimal::ZERO {
        return None;
    }

    let risk_amount = params.equity * params.risk_pct / dec!(100) * params.breaker_multiplier;
    let stop_distance = (params.entry_price - params.stop_loss_price).abs();

    if stop_distance < params.entry_price * params.min_stop_frac {
        return None;
    }

    let qty = risk_amount / stop_distance;
    let notional = qty * params.entry_price;

    let max_notional = params.equity * params.max_notional_frac;
    let (qty, capped) = if notional > max_notional {
        (max_notional / params.entry_price, true)
    } else {
        (qty, false)
    };

    if qty < dec!(0.0001) {
        return None;
    }

    let risk_amount_effective = qty * stop_distance;

    Some(SizingResult {
        qty,
        notional: qty * params.entry_price,
        risk_amount,
        risk_amount_effective,
        capped,
    })
}

// ═══════════════════════════════════════════════════════════════════
//  OFI — Order Flow Imbalance (CPU, not GPU)
// ═══════════════════════════════════════════════════════════════════
//
//  Positive OFI = net buying pressure
//  Negative OFI = net selling pressure
//  Used as entry filter: don't enter long if OFI is strongly negative

pub fn calculate_ofi(
    bids: &[(f64, f64)],
    asks: &[(f64, f64)],
    prev_bids: &[(f64, f64)],
    prev_asks: &[(f64, f64)],
    depth: usize,
) -> f64 {
    let depth = depth
        .min(bids.len())
        .min(asks.len())
        .min(prev_bids.len())
        .min(prev_asks.len());
    if depth == 0 {
        return 0.0;
    }

    let bid_delta: f64 = bids
        .iter()
        .take(depth)
        .zip(prev_bids.iter().take(depth))
        .map(|((_, sz), (_, prev_sz))| sz - prev_sz)
        .sum();

    let ask_delta: f64 = asks
        .iter()
        .take(depth)
        .zip(prev_asks.iter().take(depth))
        .map(|((_, sz), (_, prev_sz))| sz - prev_sz)
        .sum();

    bid_delta - ask_delta
}

pub fn ofi_confirms_signal(ofi: f64, is_buy: bool, threshold: f64) -> bool {
    if is_buy {
        ofi > threshold
    } else {
        ofi < -threshold
    }
}

// ═══════════════════════════════════════════════════════════════════
//  INTRADAY DRAWDOWN TRACKING — Real-time Peak-to-Trough
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default, Serialize)]
pub struct IntradayDrawdown {
    pub peak_equity: f64,
    pub current_equity: f64,
    pub drawdown_pct: f64,
    pub max_drawdown_pct: f64,
    pub drawdown_history: Vec<(i64, f64)>,
}

impl IntradayDrawdown {
    pub fn update(&mut self, equity: f64, timestamp: i64) {
        self.current_equity = equity;
        if equity > self.peak_equity {
            self.peak_equity = equity;
        }
        self.drawdown_pct = if self.peak_equity > 0.0 {
            ((self.peak_equity - equity) / self.peak_equity) * 100.0
        } else {
            0.0
        };
        if self.drawdown_pct > self.max_drawdown_pct {
            self.max_drawdown_pct = self.drawdown_pct;
        }
        self.drawdown_history.push((timestamp, self.drawdown_pct));
        if self.drawdown_history.len() > 1000 {
            self.drawdown_history.drain(..500);
        }
    }

    pub fn reset(&mut self, starting_equity: f64) {
        *self = IntradayDrawdown {
            peak_equity: starting_equity,
            current_equity: starting_equity,
            ..Default::default()
        };
    }
}

// ═══════════════════════════════════════════════════════════════════
//  SPREAD GUARD — Abort if spread is too wide
// ═══════════════════════════════════════════════════════════════════

pub fn spread_is_safe(best_bid: f64, best_ask: f64, max_spread_pct: f64) -> bool {
    if best_bid <= 0.0 || best_ask <= 0.0 {
        return false;
    }
    let mid = (best_bid + best_ask) / 2.0;
    let spread_pct = (best_ask - best_bid) / mid * 100.0;
    spread_pct <= max_spread_pct
}

// ═══════════════════════════════════════════════════════════════════
//  PORTFOLIO VaR — Parametric Value at Risk + CVaR
// ═══════════════════════════════════════════════════════════════════
//
//  Parametric (variance-covariance) approach:
//    VaR = z_score * sqrt(w' * Sigma * w)
//  where w = position weight vector, Sigma = covariance matrix.
//
//  CVaR (Expected Shortfall) estimates average loss beyond VaR
//  under normal distribution: CVaR = VaR * pdf(z) / (1 - confidence).

/// Position weight for a single asset in the portfolio.
#[derive(Debug, Clone)]
pub struct PortfolioPosition {
    pub symbol: String,
    pub notional: f64,
}

/// Compute period-over-period log returns from close prices.
pub fn compute_returns(closes: &[f64]) -> Vec<f64> {
    if closes.len() < 2 {
        return Vec::new();
    }
    closes
        .windows(2)
        .filter_map(|w| {
            if w[0] > 0.0 {
                Some((w[1] / w[0]).ln())
            } else {
                None
            }
        })
        .collect()
}

/// Build NxN covariance matrix from a map of per-symbol returns.
/// `returns_list[i]` corresponds to asset i. All vectors are aligned (same length).
/// Uses sample covariance (N-1 denominator) for unbiased estimation.
pub fn covariance_matrix(returns_list: &[&[f64]]) -> Vec<Vec<f64>> {
    let n = returns_list.len();
    if n == 0 {
        return Vec::new();
    }

    let min_len = returns_list.iter().map(|r| r.len()).min().unwrap_or(0);
    if min_len < 2 {
        return vec![vec![0.0; n]; n];
    }

    let means: Vec<f64> = returns_list
        .iter()
        .map(|r| r.iter().take(min_len).sum::<f64>() / min_len as f64)
        .collect();

    let mut cov = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in i..n {
            let val: f64 = (0..min_len)
                .map(|k| (returns_list[i][k] - means[i]) * (returns_list[j][k] - means[j]))
                .sum::<f64>()
                / (min_len - 1) as f64;
            cov[i][j] = val;
            cov[j][i] = val;
        }
    }
    cov
}

/// z-score for the normal distribution at a given confidence level.
/// Uses rational approximation (Abramowitz & Stegun 26.2.23, |error| < 4.5e-4).
fn z_score(confidence: f64) -> f64 {
    let p = 1.0 - confidence;
    if p <= 0.0 || p >= 1.0 {
        return 0.0;
    }
    let t = (-2.0 * p.ln()).sqrt();
    let c0 = 2.515517;
    let c1 = 0.802853;
    let c2 = 0.010328;
    let d1 = 1.432788;
    let d2 = 0.189269;
    let d3 = 0.001308;
    t - (c0 + c1 * t + c2 * t * t) / (1.0 + d1 * t + d2 * t * t + d3 * t * t * t)
}

/// Standard normal PDF value at x.
fn normal_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Compute parametric VaR for a portfolio.
/// Returns the VaR as a positive fraction of portfolio value (e.g. 0.02 = 2% at risk).
/// `positions` are (notional_value) per asset, `cov` is the covariance matrix.
pub fn parametric_var(
    positions: &[f64],
    cov: &[Vec<f64>],
    confidence: f64,
) -> f64 {
    let n = positions.len();
    if n == 0 || cov.len() != n {
        return 0.0;
    }

    let total_notional: f64 = positions.iter().map(|p| p.abs()).sum();
    if total_notional <= 0.0 {
        return 0.0;
    }

    let weights: Vec<f64> = positions.iter().map(|p| p.abs() / total_notional).collect();

    // portfolio variance = w' * Sigma * w
    let mut portfolio_var = 0.0;
    for i in 0..n {
        for j in 0..n {
            portfolio_var += weights[i] * weights[j] * cov[i][j];
        }
    }

    if portfolio_var < 0.0 {
        return 0.0;
    }

    let portfolio_std = portfolio_var.sqrt();
    let z = z_score(confidence);

    z * portfolio_std
}

/// CVaR (Expected Shortfall) -- average loss beyond VaR under normal assumption.
pub fn conditional_var(var_value: f64, confidence: f64) -> f64 {
    if confidence <= 0.0 || confidence >= 1.0 || var_value <= 0.0 {
        return 0.0;
    }
    let z = z_score(confidence);
    let pdf = normal_pdf(z);
    var_value * pdf / (z * (1.0 - confidence))
}

/// Check whether adding a new position would push portfolio VaR above the threshold.
/// Returns `true` if the entry should be BLOCKED.
///
/// `fail_closed`: when `true`, blocks entry if returns data is insufficient
/// (no asset has >= 2 data points). This prevents entries under risk uncertainty.
/// When `false`, missing data allows entry (legacy fail-open behavior).
pub fn var_gate_blocks_entry(
    current_positions: &[PortfolioPosition],
    new_symbol: &str,
    new_notional: f64,
    returns_map: &std::collections::HashMap<String, Vec<f64>>,
    confidence: f64,
    max_var_pct: f64,
    fail_closed: bool,
) -> bool {
    if max_var_pct <= 0.0 {
        return false;
    }

    let mut symbols: Vec<&str> = current_positions.iter().map(|p| p.symbol.as_str()).collect();
    let mut notionals: Vec<f64> = current_positions.iter().map(|p| p.notional).collect();

    if let Some(idx) = symbols.iter().position(|s| *s == new_symbol) {
        notionals[idx] += new_notional;
    } else {
        symbols.push(new_symbol);
        notionals.push(new_notional);
    }

    let returns_refs: Vec<&[f64]> = symbols
        .iter()
        .map(|s| {
            returns_map
                .get(*s)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        })
        .collect();

    let has_data = returns_refs.iter().any(|r| r.len() >= 2);
    if !has_data {
        return fail_closed;
    }

    let cov = covariance_matrix(&returns_refs);
    let proposed_var = parametric_var(&notionals, &cov, confidence);

    proposed_var > max_var_pct / 100.0
}

// ═══════════════════════════════════════════════════════════════════
//  CONCENTRATION RISK — Herfindahl-Hirschman Index (HHI)
// ═══════════════════════════════════════════════════════════════════
//
//  HHI = sum(w_i^2) where w_i = |notional_i| / total_notional
//  Range: 1/N (perfectly diversified) to 1.0 (single asset).
//  Gate: block entries that would push HHI above threshold.

/// Compute HHI from a list of position notionals.
pub fn concentration_hhi(notionals: &[f64]) -> f64 {
    let total: f64 = notionals.iter().map(|n| n.abs()).sum();
    if total <= 0.0 {
        return 0.0;
    }
    notionals
        .iter()
        .map(|n| {
            let w = n.abs() / total;
            w * w
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn calculate_var(returns: &[f64], confidence: f64) -> f64 {
        if returns.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f64> = returns.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((1.0 - confidence) * sorted.len() as f64).floor() as usize;
        sorted.get(idx).copied().unwrap_or(0.0)
    }

    fn portfolio_var(trade_pnls: &[f64], equity: f64, confidence: f64) -> f64 {
        if trade_pnls.is_empty() || equity <= 0.0 {
            return 0.0;
        }
        let returns: Vec<f64> = trade_pnls.iter().map(|pnl| pnl / equity).collect();
        calculate_var(&returns, confidence) * equity
    }

    fn fractional_kelly(win_rate: f64, avg_win: f64, avg_loss: f64, fraction: f64) -> f64 {
        if avg_loss == 0.0 || win_rate <= 0.0 || win_rate >= 1.0 {
            return 0.0;
        }
        let b = avg_win / avg_loss.abs();
        let kelly = win_rate - (1.0 - win_rate) / b;
        (kelly * fraction).clamp(0.0, 0.25)
    }

    #[test]
    fn test_breaker_levels() {
        let cb = CircuitBreaker::with_defaults();

        assert_eq!(cb.evaluate(dec!(0.5), dec!(0.0)), BreakerLevel::Normal);
        assert!(cb.can_enter());
        assert_eq!(cb.size_multiplier(), dec!(1));

        assert_eq!(cb.evaluate(dec!(3.5), dec!(1.0)), BreakerLevel::Caution);
        assert!(cb.can_enter());
        assert_eq!(cb.size_multiplier(), dec!(0.5));

        assert_eq!(cb.evaluate(dec!(5.5), dec!(2.0)), BreakerLevel::Halt);
        assert!(!cb.can_enter());

        assert_eq!(cb.evaluate(dec!(9.0), dec!(9.0)), BreakerLevel::Panic);
        assert!(!cb.can_enter());
        assert!(cb.kill_switch.load(Ordering::SeqCst));
    }

    #[test]
    fn test_breaker_reset() {
        let cb = CircuitBreaker::with_defaults();
        cb.evaluate(dec!(6.0), dec!(6.0));
        assert!(!cb.can_enter());

        cb.manual_reset();
        assert!(cb.can_enter());
        assert_eq!(cb.level(), BreakerLevel::Normal);
    }

    #[test]
    fn test_session_analytics() {
        let mut sa = SessionAnalytics::new(dec!(100000));

        sa.record_trade(dec!(500));
        assert_eq!(sa.total_trades, 1);
        assert_eq!(sa.winning_trades, 1);
        assert_eq!(sa.total_pnl, dec!(500));
        assert_eq!(sa.daily_pnl, dec!(500));

        sa.record_trade(dec!(-200));
        assert_eq!(sa.total_trades, 2);
        assert_eq!(sa.losing_trades, 1);
        assert_eq!(sa.win_rate_pct, dec!(50));
        assert!(sa.profit_factor > dec!(2));
    }

    #[test]
    fn test_daily_pnl_pct() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        sa.record_trade(dec!(-2500));
        let pct = sa.daily_pnl_pct();
        assert_eq!(pct, dec!(-2.5));
    }

    #[test]
    fn test_cooldown() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        assert!(sa.cooldown_elapsed(30));

        sa.record_entry_time();
        assert!(!sa.cooldown_elapsed(30));
        assert!(sa.cooldown_elapsed(0));
    }

    #[test]
    fn test_position_sizing_risk_based() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(100000),
            risk_pct: dec!(1),
            entry_price: dec!(200.00),
            stop_loss_price: dec!(196.00),
            breaker_multiplier: dec!(1),
            max_notional_frac: dec!(0.25),
            min_stop_frac: dec!(0.0001),
        });

        let r = result.unwrap();
        assert_eq!(r.risk_amount, dec!(1000));
        assert_eq!(r.qty, dec!(125));
        assert_eq!(r.notional, dec!(25000));
    }

    #[test]
    fn test_position_sizing_with_breaker_reduction() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(100000),
            risk_pct: dec!(2),
            entry_price: dec!(150.00),
            stop_loss_price: dec!(147.00),
            breaker_multiplier: dec!(0.5),
            max_notional_frac: dec!(0.25),
            min_stop_frac: dec!(0.0001),
        });

        let r = result.unwrap();
        assert_eq!(r.risk_amount, dec!(1000));
    }

    #[test]
    fn test_ofi() {
        let prev_bids = vec![(100.0, 50.0), (99.0, 30.0)];
        let curr_bids = vec![(100.0, 80.0), (99.0, 40.0)];
        let prev_asks = vec![(101.0, 50.0), (102.0, 30.0)];
        let curr_asks = vec![(101.0, 40.0), (102.0, 25.0)];

        let ofi = calculate_ofi(&curr_bids, &curr_asks, &prev_bids, &prev_asks, 2);
        assert!(ofi > 0.0); // bid increased more, ask decreased = buy pressure
    }

    #[test]
    fn test_spread_guard() {
        assert!(spread_is_safe(99.99, 100.01, 0.05));
        assert!(!spread_is_safe(99.50, 100.50, 0.05));
        assert!(!spread_is_safe(0.0, 100.0, 0.05));
    }

    #[test]
    fn test_calculate_var_basic() {
        let returns = vec![-0.05, -0.03, -0.01, 0.0, 0.01, 0.02, 0.03, 0.04, 0.05, 0.06];
        let var_95 = calculate_var(&returns, 0.95);
        assert!(var_95 < 0.0, "95% VaR should be negative, got {}", var_95);
        assert!((var_95 - (-0.05)).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_var_empty() {
        assert_eq!(calculate_var(&[], 0.95), 0.0);
    }

    #[test]
    fn test_portfolio_var() {
        let pnls = vec![
            -500.0, -300.0, -100.0, 0.0, 100.0, 200.0, 300.0, 400.0, 500.0, 600.0,
        ];
        let var = portfolio_var(&pnls, 100_000.0, 0.95);
        assert!(var < 0.0, "Portfolio VaR should be negative, got {}", var);
    }

    #[test]
    fn test_portfolio_var_edge_cases() {
        assert_eq!(portfolio_var(&[], 100_000.0, 0.95), 0.0);
        assert_eq!(portfolio_var(&[100.0], 0.0, 0.95), 0.0);
        assert_eq!(portfolio_var(&[100.0], -1.0, 0.95), 0.0);
    }

    #[test]
    fn test_fractional_kelly_profitable() {
        let kelly = fractional_kelly(0.6, 2.0, 1.0, 0.5);
        assert!(
            kelly > 0.0,
            "Profitable system should have positive Kelly, got {}",
            kelly
        );
        assert!(
            kelly <= 0.25,
            "Kelly should be capped at 25%, got {}",
            kelly
        );
    }

    #[test]
    fn test_fractional_kelly_edge_cases() {
        assert_eq!(fractional_kelly(0.5, 1.0, 0.0, 0.5), 0.0);
        assert_eq!(fractional_kelly(0.0, 2.0, 1.0, 0.5), 0.0);
        assert_eq!(fractional_kelly(1.0, 2.0, 1.0, 0.5), 0.0);
        assert_eq!(fractional_kelly(-0.1, 2.0, 1.0, 0.5), 0.0);
    }

    #[test]
    fn test_fractional_kelly_losing_system() {
        let kelly = fractional_kelly(0.3, 1.0, 1.0, 0.5);
        assert_eq!(kelly, 0.0, "Losing system should return 0, got {}", kelly);
    }

    #[test]
    fn test_intraday_drawdown_update() {
        let mut dd = IntradayDrawdown::default();
        dd.reset(100_000.0);

        dd.update(100_000.0, 1000);
        assert_eq!(dd.drawdown_pct, 0.0);
        assert_eq!(dd.peak_equity, 100_000.0);

        dd.update(102_000.0, 2000);
        assert_eq!(dd.peak_equity, 102_000.0);
        assert_eq!(dd.drawdown_pct, 0.0);

        dd.update(98_000.0, 3000);
        let expected_dd = ((102_000.0 - 98_000.0) / 102_000.0) * 100.0;
        assert!((dd.drawdown_pct - expected_dd).abs() < 1e-10);
        assert_eq!(dd.max_drawdown_pct, dd.drawdown_pct);
        assert_eq!(dd.drawdown_history.len(), 3);
    }

    #[test]
    fn test_intraday_drawdown_reset() {
        let mut dd = IntradayDrawdown::default();
        dd.reset(100_000.0);
        dd.update(95_000.0, 1000);
        assert!(dd.max_drawdown_pct > 0.0);

        dd.reset(110_000.0);
        assert_eq!(dd.peak_equity, 110_000.0);
        assert_eq!(dd.current_equity, 110_000.0);
        assert_eq!(dd.drawdown_pct, 0.0);
        assert_eq!(dd.max_drawdown_pct, 0.0);
        assert!(dd.drawdown_history.is_empty());
    }

    #[test]
    fn test_intraday_drawdown_history_cap() {
        let mut dd = IntradayDrawdown::default();
        dd.reset(100_000.0);
        for i in 0..1010 {
            dd.update(100_000.0 - (i as f64), i as i64);
        }
        assert!(dd.drawdown_history.len() <= 1000 + 10);
    }

    proptest! {
        #[test]
        fn position_size_never_negative(
            equity in 0.01f64..1_000_000.0,
            entry_price in 0.01f64..100_000.0,
            stop_pct in 0.001f64..0.5,
            risk_pct in 0.001f64..10.0,
        ) {
            let stop_loss_price = entry_price * (1.0 - stop_pct);
            let result = calculate_position_size(&SizingParams {
                equity: Decimal::from_f64_retain(equity).unwrap_or(dec!(1)),
                risk_pct: Decimal::from_f64_retain(risk_pct).unwrap_or(dec!(1)),
                entry_price: Decimal::from_f64_retain(entry_price).unwrap_or(dec!(1)),
                stop_loss_price: Decimal::from_f64_retain(stop_loss_price).unwrap_or(dec!(1)),
                breaker_multiplier: dec!(1),
                max_notional_frac: dec!(0.25),
                min_stop_frac: dec!(0.0001),
            });
            if let Some(r) = result {
                prop_assert!(r.qty >= Decimal::ZERO, "qty must be >= 0, got {}", r.qty);
            }
        }

        #[test]
        fn position_size_zero_equity_returns_none(
            entry_price in 0.01f64..100_000.0,
            stop_pct in 0.001f64..0.5,
            risk_pct in 0.001f64..10.0,
        ) {
            let stop_loss_price = entry_price * (1.0 - stop_pct);
            let result = calculate_position_size(&SizingParams {
                equity: Decimal::ZERO,
                risk_pct: Decimal::from_f64_retain(risk_pct).unwrap_or(dec!(1)),
                entry_price: Decimal::from_f64_retain(entry_price).unwrap_or(dec!(1)),
                stop_loss_price: Decimal::from_f64_retain(stop_loss_price).unwrap_or(dec!(1)),
                breaker_multiplier: dec!(1),
                max_notional_frac: dec!(0.25),
                min_stop_frac: dec!(0.0001),
            });
            prop_assert!(result.is_none(), "zero equity must return None");
        }

        #[test]
        fn circuit_breaker_transitions_monotone(
            losses in proptest::collection::vec(0.0f64..3.0, 1..20),
        ) {
            let cb = CircuitBreaker::with_defaults();
            let mut cumulative = 0.0f64;
            let mut highest_level = 0u8;

            let level_rank = |l: &BreakerLevel| -> u8 {
                match l {
                    BreakerLevel::Normal => 0,
                    BreakerLevel::Caution => 1,
                    BreakerLevel::Halt => 2,
                    BreakerLevel::Panic => 3,
                }
            };

            for loss in &losses {
                cumulative += loss;
                let loss_pct = Decimal::from_f64_retain(cumulative).unwrap_or(Decimal::ZERO);
                let level = cb.evaluate(loss_pct, loss_pct);
                let rank = level_rank(&level);
                prop_assert!(
                    rank >= highest_level,
                    "level went from {} to {} at cumulative loss {}",
                    highest_level, rank, cumulative,
                );
                highest_level = rank;
            }
        }
    }

    #[test]
    fn test_position_size_stop_equals_entry() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(100000),
            risk_pct: dec!(1),
            entry_price: dec!(100),
            stop_loss_price: dec!(100),
            breaker_multiplier: dec!(1),
            max_notional_frac: dec!(0.25),
            min_stop_frac: dec!(0.0001),
        });
        assert!(result.is_none());
    }

    #[test]
    fn test_position_size_negative_equity() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(-5000),
            risk_pct: dec!(1),
            entry_price: dec!(100),
            stop_loss_price: dec!(95),
            breaker_multiplier: dec!(1),
            max_notional_frac: dec!(0.25),
            min_stop_frac: dec!(0.0001),
        });
        assert!(result.is_none());
    }

    #[test]
    fn test_position_size_notional_cap() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(10000),
            risk_pct: dec!(50),
            entry_price: dec!(10),
            stop_loss_price: dec!(9),
            breaker_multiplier: dec!(1),
            max_notional_frac: dec!(0.25),
            min_stop_frac: dec!(0.0001),
        });
        let r = result.unwrap();
        assert!(r.notional <= dec!(2500));
    }

    #[test]
    fn test_sizing_capped_flag_set_when_notional_exceeds() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(10000),
            risk_pct: dec!(50),
            entry_price: dec!(10),
            stop_loss_price: dec!(9),
            breaker_multiplier: dec!(1),
            max_notional_frac: dec!(0.25),
            min_stop_frac: dec!(0.0001),
        });
        let r = result.unwrap();
        assert!(r.capped, "should be capped when risk demands more than 25% equity");
        assert!(r.notional <= dec!(2500));
    }

    #[test]
    fn test_risk_amount_effective_lt_raw_when_capped() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(10000),
            risk_pct: dec!(50),
            entry_price: dec!(10),
            stop_loss_price: dec!(9),
            breaker_multiplier: dec!(1),
            max_notional_frac: dec!(0.25),
            min_stop_frac: dec!(0.0001),
        });
        let r = result.unwrap();
        assert!(r.capped);
        assert!(
            r.risk_amount_effective < r.risk_amount,
            "effective risk {} must be < raw risk {} when capped",
            r.risk_amount_effective,
            r.risk_amount
        );
    }

    #[test]
    fn test_sizing_uncapped_effective_equals_raw() {
        let result = calculate_position_size(&SizingParams {
            equity: dec!(100000),
            risk_pct: dec!(1),
            entry_price: dec!(200),
            stop_loss_price: dec!(196),
            breaker_multiplier: dec!(1),
            max_notional_frac: dec!(0.75),
            min_stop_frac: dec!(0.0001),
        });
        let r = result.unwrap();
        assert!(!r.capped, "with 75% cap notional 50k should fit");
        assert_eq!(r.risk_amount_effective, r.risk_amount);
    }

    #[test]
    fn test_breaker_evaluate_exact_boundaries() {
        let cb = CircuitBreaker::new(dec!(3), dec!(5), dec!(8));
        assert_eq!(cb.evaluate(dec!(3), dec!(0)), BreakerLevel::Caution);
        assert_eq!(cb.evaluate(dec!(5), dec!(0)), BreakerLevel::Halt);
        assert_eq!(cb.evaluate(dec!(8), dec!(0)), BreakerLevel::Panic);
        assert_eq!(cb.evaluate(dec!(2.99), dec!(0)), BreakerLevel::Normal);
    }

    #[test]
    fn test_breaker_drawdown_triggers_panic() {
        let cb = CircuitBreaker::new(dec!(3), dec!(5), dec!(8));
        assert_eq!(cb.evaluate(dec!(0), dec!(8)), BreakerLevel::Panic);
    }

    #[test]
    fn test_session_sharpe_single_trade() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        sa.record_trade(dec!(100));
        assert_eq!(sa.sharpe_ratio(), 0.0);
    }

    #[test]
    fn test_session_sharpe_all_losses() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        sa.record_trade(dec!(-100));
        sa.record_trade(dec!(-200));
        sa.record_trade(dec!(-150));
        assert!(sa.sharpe_ratio() < 0.0);
    }

    #[test]
    fn test_session_sharpe_zero_variance() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        sa.record_trade(dec!(100));
        sa.record_trade(dec!(100));
        sa.record_trade(dec!(100));
        assert_eq!(sa.sharpe_ratio(), 0.0);
    }

    #[test]
    fn test_session_reset_daily() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        sa.record_trade(dec!(500));
        sa.reset_daily();
        assert_eq!(sa.daily_pnl, Decimal::ZERO);
        assert_eq!(sa.session_start_equity, dec!(100500));
    }

    #[test]
    fn test_session_update_equity_drawdown() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        sa.update_equity(dec!(105000));
        sa.update_equity(dec!(100000));
        assert!(sa.max_drawdown_pct > Decimal::ZERO);
        assert_eq!(sa.peak_equity, dec!(105000));
    }

    #[test]
    fn test_ofi_empty_books() {
        assert_eq!(calculate_ofi(&[], &[], &[], &[], 10), 0.0);
    }

    #[test]
    fn test_ofi_mismatched_depths() {
        let bids = vec![(100.0, 50.0)];
        let asks = vec![(101.0, 50.0), (102.0, 30.0)];
        let prev_bids = vec![(100.0, 40.0)];
        let prev_asks = vec![(101.0, 45.0)];
        let ofi = calculate_ofi(&bids, &asks, &prev_bids, &prev_asks, 5);
        assert!((ofi - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_spread_safe_zero_prices() {
        assert!(!spread_is_safe(0.0, 0.0, 0.05));
    }

    #[test]
    fn test_spread_safe_boundary() {
        assert!(spread_is_safe(99.98, 100.02, 0.05));
        assert!(!spread_is_safe(99.90, 100.10, 0.05));
    }

    #[test]
    fn test_session_restore_pnls() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        sa.restore_trade_pnls(vec![dec!(100), dec!(-50), dec!(200)]);
        assert_eq!(sa.trade_pnls().len(), 3);
    }

    // ═══════════════════════════════════════════════════════════════
    //  DRAWDOWN VELOCITY TESTS
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_dd_velocity_no_data() {
        let sa = SessionAnalytics::new(dec!(100000));
        assert_eq!(sa.dd_velocity, 0.0);
        assert!(!sa.check_dd_velocity_halt(-1.0, 300));
    }

    #[test]
    fn test_dd_velocity_stable_equity() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        for t in 0..60 {
            sa.record_dd_snapshot(1000 + t * 2);
        }
        assert!(
            sa.dd_velocity.abs() < 0.001,
            "Stable equity should have ~zero velocity, got {}",
            sa.dd_velocity
        );
        assert!(!sa.check_dd_velocity_halt(-1.0, 300));
    }

    #[test]
    fn test_dd_velocity_cascading_loss() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        // Simulate 3 minutes of losing trades: lose $500 every 10 seconds
        for i in 0..18u64 {
            let t = 1000 + i * 10;
            sa.record_trade(dec!(-500));
            sa.record_dd_snapshot(t);
        }
        // Lost $9000 over 170s (~2.83 min) from $100k peak
        // DD went from 0% to ~9%, velocity = -(9/2.83) ~ -3.18%/min
        assert!(
            sa.dd_velocity < -2.0,
            "Cascading loss should produce strong negative velocity, got {}",
            sa.dd_velocity
        );
        assert!(
            sa.check_dd_velocity_halt(-1.0, 300),
            "Should trigger halt at -1%/min threshold"
        );
    }

    #[test]
    fn test_dd_velocity_no_halt_at_safe_threshold() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        // Mild loss: $100 per 10s for 1 minute
        for i in 0..6u64 {
            let t = 1000 + i * 10;
            sa.record_trade(dec!(-100));
            sa.record_dd_snapshot(t);
        }
        // Lost $600/100k = 0.6% over 50s (~0.83 min), velocity ~ -0.72%/min
        assert!(
            !sa.check_dd_velocity_halt(-1.0, 300),
            "Mild loss should NOT trigger halt at -1%/min threshold, velocity={}",
            sa.dd_velocity
        );
    }

    #[test]
    fn test_dd_velocity_ring_eviction() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        for i in 0..200u64 {
            sa.record_dd_snapshot(1000 + i);
        }
        assert!(
            sa.dd_velocity_ring.len() <= 120,
            "Ring should cap at 120 entries, got {}",
            sa.dd_velocity_ring.len()
        );
    }

    #[test]
    fn test_dd_velocity_window_secs() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        // Phase 1: stable for 5 minutes
        for i in 0..150u64 {
            sa.record_dd_snapshot(1000 + i * 2);
        }
        // Ring is now full with stable data. Lose money in last 30s
        for i in 0..15u64 {
            let t = 1300 + i * 2;
            sa.record_trade(dec!(-400));
            sa.record_dd_snapshot(t);
        }
        // With 30s window, velocity should be very negative
        assert!(
            sa.check_dd_velocity_halt(-1.0, 30),
            "Should halt with 30s window after sudden losses"
        );
    }

    #[test]
    fn test_dd_velocity_positive_threshold_never_halts() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        for i in 0..20u64 {
            sa.record_trade(dec!(-1000));
            sa.record_dd_snapshot(1000 + i * 10);
        }
        assert!(
            !sa.check_dd_velocity_halt(0.0, 300),
            "Zero threshold should never halt"
        );
        assert!(
            !sa.check_dd_velocity_halt(1.0, 300),
            "Positive threshold should never halt"
        );
    }

    #[test]
    fn test_dd_velocity_ring_snapshot_roundtrip() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        for i in 0..10u64 {
            sa.record_trade(dec!(-200));
            sa.record_dd_snapshot(1000 + i * 10);
        }
        let original_velocity = sa.dd_velocity;
        let original_ring_len = sa.dd_velocity_ring.len();
        assert!(original_ring_len > 0, "ring should have entries");

        let snapshot = sa.dd_velocity_ring_snapshot();
        assert_eq!(snapshot.len(), original_ring_len);

        let mut sa2 = SessionAnalytics::new(dec!(100000));
        sa2.restore_trade_pnls(vec![dec!(-200); 10]);
        sa2.restore_dd_velocity_ring(snapshot);
        assert_eq!(sa2.dd_velocity_ring.len(), original_ring_len);
        assert!(
            (sa2.dd_velocity - original_velocity).abs() < 1e-10,
            "Restored velocity ({}) should match original ({})",
            sa2.dd_velocity,
            original_velocity
        );
    }

    #[test]
    fn test_dd_velocity_ring_snapshot_empty() {
        let sa = SessionAnalytics::new(dec!(100000));
        let snapshot = sa.dd_velocity_ring_snapshot();
        assert!(snapshot.is_empty());

        let mut sa2 = SessionAnalytics::new(dec!(100000));
        sa2.restore_dd_velocity_ring(vec![]);
        assert_eq!(sa2.dd_velocity, 0.0);
    }

    // ═══════════════════════════════════════════════════════════════
    //  Portfolio VaR Tests
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_compute_returns_basic() {
        let closes = vec![100.0, 105.0, 102.0, 108.0];
        let rets = compute_returns(&closes);
        assert_eq!(rets.len(), 3);
        let expected_0 = (105.0_f64 / 100.0).ln();
        assert!((rets[0] - expected_0).abs() < 1e-12);
    }

    #[test]
    fn test_compute_returns_empty_and_single() {
        assert!(compute_returns(&[]).is_empty());
        assert!(compute_returns(&[100.0]).is_empty());
    }

    #[test]
    fn test_covariance_matrix_single_asset() {
        let returns = vec![0.01, -0.02, 0.015, -0.005, 0.008];
        let cov = covariance_matrix(&[&returns]);
        assert_eq!(cov.len(), 1);
        assert_eq!(cov[0].len(), 1);
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let expected_var: f64 = returns
            .iter()
            .map(|r| (r - mean).powi(2))
            .sum::<f64>()
            / (returns.len() - 1) as f64;
        assert!(
            (cov[0][0] - expected_var).abs() < 1e-15,
            "Single asset covariance should equal variance"
        );
    }

    #[test]
    fn test_covariance_matrix_symmetric() {
        let r1 = vec![0.01, -0.02, 0.015, -0.005, 0.008];
        let r2 = vec![0.005, -0.01, 0.02, 0.003, -0.007];
        let cov = covariance_matrix(&[&r1, &r2]);
        assert_eq!(cov.len(), 2);
        assert!(
            (cov[0][1] - cov[1][0]).abs() < 1e-15,
            "Covariance matrix must be symmetric"
        );
    }

    #[test]
    fn test_single_position_var() {
        let returns = vec![0.01, -0.02, 0.015, -0.005, 0.008, -0.012, 0.003, 0.007, -0.009, 0.011];
        let cov = covariance_matrix(&[&returns]);
        let var95 = parametric_var(&[10000.0], &cov, 0.95);
        assert!(var95 > 0.0, "VaR must be positive, got {}", var95);
        let var99 = parametric_var(&[10000.0], &cov, 0.99);
        assert!(
            var99 > var95,
            "99% VaR ({}) must exceed 95% VaR ({})",
            var99,
            var95
        );
    }

    #[test]
    fn test_two_correlated_assets_var() {
        // Two perfectly correlated assets: returns are identical
        let r = vec![0.01, -0.02, 0.015, -0.005, 0.008, -0.012, 0.003];
        let cov = covariance_matrix(&[&r, &r]);
        let var_combined = parametric_var(&[5000.0, 5000.0], &cov, 0.95);
        let var_single = parametric_var(&[10000.0], &covariance_matrix(&[&r]), 0.95);
        assert!(
            (var_combined - var_single).abs() < 1e-10,
            "Perfectly correlated 50/50 split should equal single position VaR: combined={}, single={}",
            var_combined,
            var_single
        );
    }

    #[test]
    fn test_uncorrelated_assets_diversification_benefit() {
        let r1 = vec![0.01, -0.02, 0.015, -0.005, 0.008, -0.012, 0.003, 0.007, -0.009, 0.011];
        // Construct uncorrelated returns by reversing
        let r2: Vec<f64> = r1.iter().rev().copied().collect();
        let cov = covariance_matrix(&[&r1, &r2]);

        let var_diversified = parametric_var(&[5000.0, 5000.0], &cov, 0.95);
        let var_concentrated = parametric_var(
            &[10000.0],
            &covariance_matrix(&[&r1]),
            0.95,
        );
        assert!(
            var_diversified < var_concentrated,
            "Diversified VaR ({}) should be less than concentrated ({})",
            var_diversified,
            var_concentrated
        );
    }

    #[test]
    fn test_empty_portfolio_zero_var() {
        let var = parametric_var(&[], &[], 0.95);
        assert_eq!(var, 0.0);
    }

    #[test]
    fn test_cvar_exceeds_var() {
        let returns = vec![0.01, -0.02, 0.015, -0.005, 0.008, -0.012, 0.003, 0.007, -0.009, 0.011];
        let cov = covariance_matrix(&[&returns]);
        let var95 = parametric_var(&[10000.0], &cov, 0.95);
        let cvar95 = conditional_var(var95, 0.95);
        assert!(
            cvar95 > var95,
            "CVaR ({}) must exceed VaR ({})",
            cvar95,
            var95
        );
    }

    #[test]
    fn test_var_gate_blocks_when_exceeded() {
        let mut returns_map = std::collections::HashMap::new();
        // High-volatility returns so VaR is large
        returns_map.insert(
            "VOLATILE".to_string(),
            vec![0.05, -0.06, 0.07, -0.04, 0.05, -0.08, 0.06, -0.03, 0.04, -0.05],
        );
        let positions = vec![PortfolioPosition {
            symbol: "VOLATILE".to_string(),
            notional: 10000.0,
        }];
        let blocked = var_gate_blocks_entry(
            &positions,
            "VOLATILE",
            90000.0,
            &returns_map,
            0.99,
            0.5,
            false,
        );
        assert!(blocked, "Adding 90k to a volatile position should breach 0.5% VaR");
    }

    #[test]
    fn test_var_gate_allows_under_threshold() {
        let mut returns_map = std::collections::HashMap::new();
        // Low-volatility returns
        returns_map.insert(
            "STABLE".to_string(),
            vec![0.001, -0.001, 0.002, -0.001, 0.001, -0.002, 0.001, -0.001, 0.001, -0.001],
        );
        let positions = vec![PortfolioPosition {
            symbol: "STABLE".to_string(),
            notional: 5000.0,
        }];
        let blocked = var_gate_blocks_entry(
            &positions,
            "STABLE",
            1000.0,
            &returns_map,
            0.95,
            5.0,
            false,
        );
        assert!(!blocked, "Small add to stable asset should not breach 5% VaR");
    }

    // ═══════════════════════════════════════════════════════════════
    //  Concentration HHI Tests
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_hhi_single_position() {
        let hhi = concentration_hhi(&[10000.0]);
        assert!((hhi - 1.0).abs() < 1e-12, "Single position HHI must be 1.0");
    }

    #[test]
    fn test_hhi_equal_positions() {
        let hhi = concentration_hhi(&[5000.0, 5000.0, 5000.0, 5000.0]);
        let expected = 4.0 * (0.25_f64).powi(2);
        assert!(
            (hhi - expected).abs() < 1e-12,
            "4 equal positions: HHI should be 0.25, got {}",
            hhi
        );
    }

    #[test]
    fn test_hhi_empty_portfolio() {
        assert_eq!(concentration_hhi(&[]), 0.0);
    }

    // ═══════════════════════════════════════════════════════════════
    //  VaR Fail-Closed Policy Tests (S04.1)
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_var_gate_fail_closed_blocks_without_data() {
        let returns_map = std::collections::HashMap::new();
        let positions = vec![PortfolioPosition {
            symbol: "UNKNOWN".to_string(),
            notional: 5000.0,
        }];
        let blocked = var_gate_blocks_entry(
            &positions, "UNKNOWN", 5000.0, &returns_map, 0.95, 2.0, true,
        );
        assert!(blocked, "fail_closed=true must block entry when no returns data exists");
    }

    #[test]
    fn test_var_gate_fail_open_allows_without_data() {
        let returns_map = std::collections::HashMap::new();
        let positions = vec![PortfolioPosition {
            symbol: "UNKNOWN".to_string(),
            notional: 5000.0,
        }];
        let blocked = var_gate_blocks_entry(
            &positions, "UNKNOWN", 5000.0, &returns_map, 0.95, 2.0, false,
        );
        assert!(!blocked, "fail_closed=false must allow entry when no returns data exists (legacy)");
    }

    #[test]
    fn test_incremental_vs_full_rescan() {
        let equity = dec!(10000);
        let pnls = vec![
            dec!(50), dec!(-30), dec!(120), dec!(-15), dec!(80),
            dec!(-60), dec!(25), dec!(-10), dec!(200), dec!(-5),
            dec!(35), dec!(-90), dec!(45), dec!(-20), dec!(150),
            dec!(-40), dec!(10), dec!(-70), dec!(65), dec!(-8),
        ];

        let mut incremental = SessionAnalytics::new(equity);
        for p in &pnls {
            incremental.record_trade(*p);
        }

        let mut restored = SessionAnalytics::new(equity);
        restored.total_trades = pnls.len() as u32;
        restored.winning_trades = pnls.iter().filter(|p| **p > Decimal::ZERO).count() as u32;
        restored.losing_trades = pnls.iter().filter(|p| **p < Decimal::ZERO).count() as u32;
        restored.restore_trade_pnls(pnls);

        assert_eq!(incremental.profit_factor, restored.profit_factor,
            "profit_factor mismatch: incremental vs restored");
        assert_eq!(incremental.avg_win, restored.avg_win,
            "avg_win mismatch: incremental vs restored");
        assert_eq!(incremental.avg_loss, restored.avg_loss,
            "avg_loss mismatch: incremental vs restored");
        assert_eq!(incremental.win_rate_pct, restored.win_rate_pct,
            "win_rate_pct mismatch: incremental vs restored");
        assert_eq!(incremental.running_gross_wins, restored.running_gross_wins,
            "running_gross_wins mismatch");
        assert_eq!(incremental.running_gross_losses, restored.running_gross_losses,
            "running_gross_losses mismatch");
    }

    /// FIX-6 HIGH-17: trade_pnls VecDeque is capped at TRADE_PNLS_CAP.
    #[test]
    fn trade_pnls_capped_at_2000() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        for i in 0..2500 {
            sa.record_trade(Decimal::from(i % 10));
        }
        assert_eq!(sa.trade_pnls().len(), TRADE_PNLS_CAP);
        assert_eq!(sa.total_trades, 2500);
    }

    /// FIX-6 HIGH-17: Incremental Sharpe (Welford) matches full-scan Sharpe.
    #[test]
    fn sharpe_incremental_matches_full_scan() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        let pnls = vec![
            dec!(50), dec!(-30), dec!(120), dec!(-15), dec!(80),
            dec!(-40), dec!(200), dec!(-100), dec!(35), dec!(-20),
        ];
        for p in &pnls {
            sa.record_trade(*p);
        }

        let returns: Vec<f64> = pnls.iter().map(|d| d.to_f64().unwrap_or(0.0)).collect();
        let n = returns.len() as f64;
        let mean = returns.iter().sum::<f64>() / n;
        let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        let expected = if std > 1e-12 { mean / std * n.sqrt() } else { 0.0 };

        let actual = sa.sharpe_ratio();
        assert!(
            (actual - expected).abs() < 1e-6,
            "Sharpe mismatch: welford={}, full={}",
            actual,
            expected
        );
    }

    /// FIX-6 HIGH-17: restore_trade_pnls caps to TRADE_PNLS_CAP.
    #[test]
    fn restore_trade_pnls_caps_to_limit() {
        let mut sa = SessionAnalytics::new(dec!(100000));
        let pnls: Vec<Decimal> = (0..3000).map(|i| Decimal::from(i % 7)).collect();
        sa.restore_trade_pnls(pnls);
        assert_eq!(sa.trade_pnls().len(), TRADE_PNLS_CAP);
    }
}
