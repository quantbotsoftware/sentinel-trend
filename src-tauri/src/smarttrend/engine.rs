use serde::{Deserialize, Serialize};

use crate::config::SmartTrendConfig;
use crate::state::{Direction, FsmPhase, MarkerKind, SmartTrendMarker, SymbolFsm};
use crate::smarttrend::strategies::MarketRegime;

pub const ENTRY_CONFIRM_TARGET: u32 = 2;


/// S36 (2026-05-28) — per-TF confirmation target.
///
/// Default Confirming → Active requires 2 consecutive bullish bars.
/// On 5m and 10m timeframes this requires the catalyst to sustain
/// momentum for 10-20 minutes before the FSM transitions, which
/// excluded most real intraday moves and left those charts empty
/// of markers (live evidence 2026-05-28: 1 of 13 watchlist symbols
/// had 5m markers, 0 of 13 had 10m markers — strictly worse than
/// the per-TF FSM keying could provide).
///
/// Lowering the target to 1 bar on 5m/10m means a single strong
/// bullish bar transitions the FSM into Active and emits an Entry
/// marker on those TFs — the desired manual-trading reference.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Timeframe {
    M1 = 1,
    M5 = 5,
    M10 = 10,
    M15 = 15,
    M30 = 30,
    H1 = 60,
    H4 = 240,
    D1 = 255,
}

impl std::fmt::Display for Timeframe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Timeframe {
    pub fn as_str(self) -> &'static str {
        match self {
            Timeframe::M1 => "1m",
            Timeframe::M5 => "5m",
            Timeframe::M10 => "10m",
            Timeframe::M15 => "15m",
            Timeframe::M30 => "30m",
            Timeframe::H1 => "1h",
            Timeframe::H4 => "4h",
            Timeframe::D1 => "1d",
        }
    }

    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "1m" => Ok(Timeframe::M1),
            "5m" => Ok(Timeframe::M5),
            "10m" => Ok(Timeframe::M10),
            "15m" => Ok(Timeframe::M15),
            "30m" => Ok(Timeframe::M30),
            "1h" => Ok(Timeframe::H1),
            "4h" => Ok(Timeframe::H4),
            "1d" => Ok(Timeframe::D1),
            _ => Err(format!("Unknown timeframe: {s}")),
        }
    }
}

use crate::ml::hawkes_core::HawkesState;

pub fn confirm_target_for_tf(tf: Timeframe) -> u32 {
    match tf {
        Timeframe::M5 | Timeframe::M10 => 1,
        _ => ENTRY_CONFIRM_TARGET,
    }
}

use super::markers::create_marker;
use super::pillars::evaluate_momentum;

#[derive(Debug, Clone)]
pub struct MomentumInput {
    pub rvol: f64,
    pub roc: f64,
}

#[derive(Debug, Clone)]
pub struct SmartTrendInput<'a> {
    pub momentum_input: MomentumInput,
    pub current_price: f64,
    pub prev_close: f64,
    pub bar_time: i64,
    pub timeframe: Timeframe,
    pub symbol: &'a str,
    pub atr: f64,
    pub regime: MarketRegime,
    pub macro_trend_bullish: bool,
    pub above_ema_fast: bool,
    pub above_ema_slow: bool,
    pub news_sentiment: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PillarDirection {
    Bullish,
    Bearish,
    Neutral,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PillarScore {
    pub score: f64,
    pub direction: PillarDirection,
    pub confidence: f64,
    pub stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmartTrendStrength {
    Extreme,
    Strong,
    Moderate,
    Weak,
}

pub fn classify_strength(combined_score: f64) -> SmartTrendStrength {
    let magnitude = (combined_score - 50.0).abs();
    if magnitude >= 25.0 {
        SmartTrendStrength::Extreme
    } else if magnitude >= 15.0 {
        SmartTrendStrength::Strong
    } else if magnitude >= 5.0 {
        SmartTrendStrength::Moderate
    } else {
        SmartTrendStrength::Weak
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PillarDiagnostics {
    pub momentum_score: PillarScore,
    pub combined_score: f64,
    pub combined_direction: PillarDirection,
    pub strength: SmartTrendStrength,
    pub hawkes_intensity: f64,
}

fn is_green_bar(input: &SmartTrendInput) -> bool {
    input.current_price > input.prev_close
}

fn is_red_bar(input: &SmartTrendInput) -> bool {
    input.current_price < input.prev_close
}

fn is_high_quality_long_entry(input: &SmartTrendInput, momentum: &PillarScore, min_momentum: f64) -> bool {
    is_green_bar(input) && momentum.score > min_momentum && !momentum.stale
}

fn entry_momentum_floor_for_tf(base: f64, tf: Timeframe) -> f64 {
    match tf {
        Timeframe::M1 | Timeframe::M5 | Timeframe::M10 => base.min(30.0),
        Timeframe::M15 | Timeframe::M30 => base.min(45.0),
        _ => base,
    }
}

#[inline(always)]
pub fn is_outside_rth_unix(timestamp_secs: i64) -> bool {
    if timestamp_secs <= 0 { return true; }
    const SECONDS_PER_DAY: i64 = 86400;
    let days_since_epoch = timestamp_secs / SECONDS_PER_DAY;
    let seconds_in_day = timestamp_secs % SECONDS_PER_DAY;
    
    let mut year = 1970 + (days_since_epoch / 365);
    let mut leap_years = (year - 1969) / 4 - (year - 1901) / 100 + (year - 1601) / 400;
    let mut days_to_start_of_year = (year - 1970) * 365 + leap_years;
    if days_to_start_of_year > days_since_epoch {
        year -= 1;
        leap_years = (year - 1969) / 4 - (year - 1901) / 100 + (year - 1601) / 400;
        days_to_start_of_year = (year - 1970) * 365 + leap_years;
    }
    
    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let days_before_march = if is_leap { 60 } else { 59 };
    let days_before_nov = if is_leap { 305 } else { 304 };
    
    let march_1_epoch_day = days_to_start_of_year + days_before_march;
    let march_1_weekday = (march_1_epoch_day + 4) % 7;
    let dst_start_day = 14 - ((march_1_weekday + 6) % 7);
    
    let nov_1_epoch_day = days_to_start_of_year + days_before_nov;
    let nov_1_weekday = (nov_1_epoch_day + 4) % 7;
    let dst_end_day = 1 + ((7 - nov_1_weekday) % 7);
    
    let dst_start_epoch_day = march_1_epoch_day + dst_start_day - 1;
    let dst_end_epoch_day = nov_1_epoch_day + dst_end_day - 1;
    
    let is_dst = if days_since_epoch > dst_start_epoch_day && days_since_epoch < dst_end_epoch_day {
        true
    } else if days_since_epoch == dst_start_epoch_day {
        seconds_in_day >= 7 * 3600
    } else if days_since_epoch == dst_end_epoch_day {
        seconds_in_day < 6 * 3600
    } else {
        false
    };
    
    let offset = if is_dst { 14400 } else { 18000 };
    let local_seconds = (seconds_in_day - offset + 86400) % 86400;
    
    let local_time = timestamp_secs - offset;
    let local_days = local_time / SECONDS_PER_DAY;
    let weekday = (local_days + 4) % 7;
    if weekday == 0 || weekday == 6 {
        return true;
    }
    
    !(34200..57600).contains(&local_seconds)
}

fn reentry_min_momentum(base: f64, tf: Timeframe, outside_rth: bool) -> f64 {
    let bump = if outside_rth {
        match tf {
            Timeframe::M1 | Timeframe::M5 | Timeframe::M10 => 0.0,
            _ => 3.0,
        }
    } else {
        match tf {
            Timeframe::M1 => 8.0,
            Timeframe::M5 => 5.0,
            _ => 3.0,
        }
    };
    (base + bump).min(90.0)
}

fn cooldown_bars_for_tf(base: u32, tf: Timeframe, outside_rth: bool) -> u32 {
    if outside_rth {
        match tf {
            Timeframe::M1 | Timeframe::M5 | Timeframe::M10 => 1,
            _ => base,
        }
    } else {
        match tf {
            Timeframe::M1 => base + 3,
            Timeframe::M5 => base + 1,
            Timeframe::M15 | Timeframe::M30 => base + 1,
            _ => base,
        }
    }
}

fn exit_bars_for_tf(base: u32, tf: Timeframe) -> u32 {
    match tf {
        Timeframe::M1 => base,
        Timeframe::M5 => base + 1,
        Timeframe::M15 | Timeframe::M30 => base,
        _ => base.max(1),
    }
}

#[derive(Debug, Clone)]
pub struct EvalResult {
    pub new_fsm: SymbolFsm,
    pub marker: Option<SmartTrendMarker>,
    pub diagnostics: PillarDiagnostics,
}

pub fn evaluate<'a>(input: &SmartTrendInput<'a>, fsm: &SymbolFsm, cfg: &SmartTrendConfig) -> EvalResult {
    let momentum = evaluate_momentum(&input.momentum_input);
    // F0: inlined former FsmEstimator (theater stubs estimator/vpin/entropy removed).
    let combined = momentum.score;
    let entry_min_momentum =
        entry_momentum_floor_for_tf(cfg.entry_min_momentum, input.timeframe);
    let entry_ok = is_high_quality_long_entry(input, &momentum, entry_min_momentum);
    let exit_bars_needed = exit_bars_for_tf(cfg.exit_confirm_bars.max(1), input.timeframe);
    let outside_rth = is_outside_rth_unix(input.bar_time);
    let cooldown_needed =
        cooldown_bars_for_tf(cfg.reentry_cooldown_bars, input.timeframe, outside_rth);
    let reentry_momentum =
        reentry_min_momentum(entry_min_momentum, input.timeframe, outside_rth);

    let mut new_fsm = fsm.clone();
    let bar_time_ns = input.bar_time * 1_000_000_000;
    
    // Hawkes process intensity update
    let mut hawkes = HawkesState {
        last_event_time_ns: new_fsm.last_event_time_ns,
        current_intensity: new_fsm.hawkes_intensity,
        mu: 0.05,
        alpha: 0.4,
        beta: 1.5,
    };
    new_fsm.hawkes_intensity = hawkes.process_tick(bar_time_ns, input.momentum_input.rvol);
    new_fsm.last_event_time_ns = hawkes.last_event_time_ns;

    let diag = PillarDiagnostics {
        momentum_score: momentum,
        combined_score: combined,
        combined_direction: momentum.direction,
        strength: classify_strength(combined),
        hawkes_intensity: new_fsm.hawkes_intensity,
    };

    let mut marker = None;

    if new_fsm.prev_close <= 0.0 {
        new_fsm.prev_close = input.current_price;
        return EvalResult {
            new_fsm,
            marker,
            diagnostics: diag,
        };
    }

    if new_fsm.phase == FsmPhase::Stalking {
        new_fsm.phase = FsmPhase::Idle;
        new_fsm.confirm_count = 0;
        new_fsm.cooldown_bars = 0;
        new_fsm.entry_price = None;
        new_fsm.entry_time = None;
        new_fsm.peak_price = 0.0;
    }

    match new_fsm.phase {
        FsmPhase::Idle => {
            new_fsm.score = combined;
            new_fsm.cooldown_bars = 0;
            if entry_ok && input.macro_trend_bullish {
                let target = confirm_target_for_tf(input.timeframe);
                if target <= 1 {
                    new_fsm.phase = FsmPhase::Active;
                    new_fsm.entry_price = Some(input.current_price);
                    new_fsm.entry_time = Some(input.bar_time);
                    new_fsm.peak_price = input.current_price;
                    new_fsm.confirm_count = 0;
                    marker = Some(create_marker(
                        MarkerKind::Entry,
                        Direction::Long,
                        input.bar_time,
                        input.current_price,
                        combined,
                        input.timeframe.as_str(),
                        input.symbol,
                    ));
                } else {
                    new_fsm.phase = FsmPhase::Confirming;
                    new_fsm.confirm_count = 1;
                }
            } else {
                new_fsm.confirm_count = 0;
            }
        }
        FsmPhase::Confirming => {
            new_fsm.score = combined;
            if entry_ok {
                new_fsm.confirm_count = new_fsm.confirm_count.saturating_add(1);
                let target = confirm_target_for_tf(input.timeframe);
                if new_fsm.confirm_count >= target {
                    new_fsm.phase = FsmPhase::Active;
                    new_fsm.entry_price = Some(input.current_price);
                    new_fsm.entry_time = Some(input.bar_time);
                    new_fsm.peak_price = input.current_price;
                    new_fsm.confirm_count = 0;
                    marker = Some(create_marker(
                        MarkerKind::Entry,
                        Direction::Long,
                        input.bar_time,
                        input.current_price,
                        combined,
                        input.timeframe.as_str(),
                        input.symbol,
                    ));
                }
            } else if is_green_bar(input) {
                // Green bar but momentum below threshold -- hold Confirming, don't reset
            } else if input.above_ema_slow {
                new_fsm.phase = FsmPhase::Pullback;
                new_fsm.confirm_count = 0;
                new_fsm.cooldown_bars = 0;
            } else {
                new_fsm.phase = FsmPhase::Idle;
                new_fsm.confirm_count = 0;
                new_fsm.entry_price = None;
                new_fsm.entry_time = None;
            }
        }
        FsmPhase::Pullback => {
            new_fsm.score = combined;
            new_fsm.cooldown_bars = new_fsm.cooldown_bars.saturating_add(1);

            let decay_factor = (1.0 - 0.1 * (new_fsm.cooldown_bars.saturating_sub(1) as f64)).max(0.5);
            let decayed_min_momentum = entry_min_momentum * decay_factor;
            let pullback_reclaim_ok = is_high_quality_long_entry(input, &momentum, decayed_min_momentum);

            if pullback_reclaim_ok && input.macro_trend_bullish {
                new_fsm.phase = FsmPhase::Active;
                new_fsm.entry_price = Some(input.current_price);
                new_fsm.entry_time = Some(input.bar_time);
                new_fsm.peak_price = input.current_price;
                new_fsm.confirm_count = 0;
                new_fsm.cooldown_bars = 0;
                marker = Some(create_marker(
                    MarkerKind::Entry,
                    Direction::Long,
                    input.bar_time,
                    input.current_price,
                    combined,
                    input.timeframe.as_str(),
                    input.symbol,
                ));
            } else if !input.above_ema_slow || new_fsm.cooldown_bars >= 5 {
                new_fsm.phase = FsmPhase::Idle;
                new_fsm.confirm_count = 0;
                new_fsm.cooldown_bars = 0;
                new_fsm.entry_price = None;
                new_fsm.entry_time = None;
            }
        }
        FsmPhase::Active => {
            new_fsm.score = combined;
            if input.current_price > new_fsm.peak_price {
                new_fsm.peak_price = input.current_price;
            }

            let is_climax = input.momentum_input.rvol > 3.0 && is_green_bar(input);
            let climax_active = new_fsm.cooldown_bars == 1 || is_climax;
            if is_climax {
                new_fsm.cooldown_bars = 1;
            }

            let mut k = if climax_active {
                0.5
            } else {
                match input.regime {
                    MarketRegime::Trending => 2.0,
                    MarketRegime::Volatile | MarketRegime::Panic => 2.8,
                    MarketRegime::Calm => 1.2,
                }
            };

            if !climax_active {
                if let Some(sentiment) = input.news_sentiment {
                    if sentiment > 0.8 {
                        k *= 1.5;
                    } else if sentiment < -0.5 {
                        k = 1.0;
                    }
                }
            }

            let trail_triggered = if input.atr > 0.0 {
                new_fsm.peak_price > 0.0
                    && input.current_price < new_fsm.peak_price - input.atr * k
            } else {
                let drop_pct = if climax_active {
                    0.2
                } else {
                    match input.regime {
                        MarketRegime::Trending => 0.8,
                        MarketRegime::Volatile | MarketRegime::Panic => 1.2,
                        MarketRegime::Calm => 0.5,
                    }
                };
                new_fsm.peak_price > 0.0
                    && input.current_price < new_fsm.peak_price * (1.0 - drop_pct / 100.0)
            };

            let should_exit = if trail_triggered {
                true
            } else if is_red_bar(input) {
                new_fsm.confirm_count = new_fsm.confirm_count.saturating_add(1);
                new_fsm.confirm_count >= exit_bars_needed
            } else {
                new_fsm.confirm_count = 0;
                false
            };

            if should_exit {
                marker = Some(create_marker(
                    MarkerKind::Exit,
                    Direction::Long,
                    input.bar_time,
                    input.current_price,
                    combined,
                    input.timeframe.as_str(),
                    input.symbol,
                ));
                new_fsm.phase = FsmPhase::Exit;
                new_fsm.confirm_count = 0;
                new_fsm.cooldown_bars = 0;
                new_fsm.entry_price = None;
                new_fsm.entry_time = None;
                new_fsm.peak_price = 0.0;
            }
        }
        FsmPhase::Exit => {
            new_fsm.score = combined;
            new_fsm.cooldown_bars = new_fsm.cooldown_bars.saturating_add(1);
            let reentry_ok = is_green_bar(input)
                && momentum.score > reentry_momentum
                && !momentum.stale;
            if new_fsm.cooldown_bars > cooldown_needed && reentry_ok && input.macro_trend_bullish {
                let target = confirm_target_for_tf(input.timeframe);
                if target <= 1 {
                    new_fsm.phase = FsmPhase::Active;
                    new_fsm.entry_price = Some(input.current_price);
                    new_fsm.entry_time = Some(input.bar_time);
                    new_fsm.peak_price = input.current_price;
                    new_fsm.confirm_count = 0;
                    new_fsm.cooldown_bars = 0;
                    marker = Some(create_marker(
                        MarkerKind::Entry,
                        Direction::Long,
                        input.bar_time,
                        input.current_price,
                        combined,
                        input.timeframe.as_str(),
                        input.symbol,
                    ));
                } else {
                    new_fsm.phase = FsmPhase::Confirming;
                    new_fsm.confirm_count = 1;
                    new_fsm.cooldown_bars = 0;
                }
            }
        }
        FsmPhase::Stalking => {
            unreachable!("FsmPhase::Stalking is normalized to Idle at function entry; legacy snapshot data only")
        }
    }

    new_fsm.prev_close = input.current_price;

    EvalResult {
        new_fsm,
        marker,
        diagnostics: diag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SmartTrendConfig;

    // S36 — per-TF confirmation target (1m=2, 5m/10m=1, others=2)
    #[test]
    fn confirm_target_5m_and_10m_relaxed_to_one_bar() {
        assert_eq!(confirm_target_for_tf(Timeframe::M5), 1);
        assert_eq!(confirm_target_for_tf(Timeframe::M10), 1);
    }

    #[test]
    fn confirm_target_1m_keeps_two_bar_default() {
        assert_eq!(confirm_target_for_tf(Timeframe::M1), 2);
    }

    #[test]
    fn confirm_target_higher_tfs_keep_default() {
        assert_eq!(confirm_target_for_tf(Timeframe::M15), 2);
        assert_eq!(confirm_target_for_tf(Timeframe::M30), 2);
        assert_eq!(confirm_target_for_tf(Timeframe::H1), 2);
        assert_eq!(confirm_target_for_tf(Timeframe::H4), 2);
        assert_eq!(confirm_target_for_tf(Timeframe::D1), 2);
    }

    /// S36 — One bullish 5m bar should transition Idle → Confirming → Active
    /// in a single step (target=1) and emit an Entry marker. Pre-S36 this
    /// required two consecutive bullish bars (10 minutes wall-clock) which
    /// missed most catalyst-driven 5m moves.
    #[test]
    fn s36_5m_transitions_active_in_single_bar() {
        let cfg = SmartTrendConfig::default();
        let mut input = green_bar_input(100.0, 102.0, 1_700_000_000);
        input.timeframe = Timeframe::M5;

        let result = evaluate(&input, &seeded_idle_fsm(100.0), &cfg);

        // First bar: Idle → Confirming with confirm_count=1, then immediately
        // promoted to Active because 5m target=1.
        assert_eq!(
            result.new_fsm.phase,
            FsmPhase::Active,
            "5m must reach Active in one bar (target=1)"
        );
        assert!(
            result.marker.as_ref().map_or(false, |m| m.kind == MarkerKind::Entry),
            "Entry marker emitted on the same bar"
        );
    }

    #[test]
    fn s36_1m_still_requires_two_bars() {
        let cfg = SmartTrendConfig::default();
        let mut input = green_bar_input(100.0, 102.0, 1_700_000_000);
        input.timeframe = Timeframe::M1;

        let result = evaluate(&input, &seeded_idle_fsm(100.0), &cfg);

        // 1m: target=2, so first bullish bar only takes us to Confirming.
        assert_eq!(
            result.new_fsm.phase,
            FsmPhase::Confirming,
            "1m must remain at Confirming after one bar"
        );
        assert!(
            result.marker.is_none(),
            "1m: no Entry marker until second bar"
        );
    }

    fn input(
        prev_close: f64,
        current_price: f64,
        rvol: f64,
        roc: f64,
        bar_time: i64,
    ) -> SmartTrendInput<'static> {
        SmartTrendInput {
            momentum_input: MomentumInput { rvol, roc },
            current_price,
            prev_close,
            bar_time,
            timeframe: Timeframe::M5,
            symbol: "AAPL",
            atr: 0.0,
            regime: MarketRegime::Calm,
            macro_trend_bullish: true,
            above_ema_fast: true,
            above_ema_slow: true,
            news_sentiment: None,
        }
    }

    fn green_bar_input(prev_close: f64, current_price: f64, bar_time: i64) -> SmartTrendInput<'static> {
        input(prev_close, current_price, 1.5, 2.0, bar_time)
    }

    fn red_bar_input(prev_close: f64, current_price: f64, bar_time: i64) -> SmartTrendInput<'static> {
        input(prev_close, current_price, 1.5, -2.0, bar_time)
    }

    fn stale_green_input(prev_close: f64, current_price: f64, bar_time: i64) -> SmartTrendInput<'static> {
        input(prev_close, current_price, 0.0, 0.0, bar_time)
    }

    fn seeded_idle_fsm(prev_close: f64) -> SymbolFsm {
        SymbolFsm {
            prev_close,
            ..SymbolFsm::default()
        }
    }

    #[test]
    fn first_eval_seeds_prev_close() {
        let cfg = SmartTrendConfig::default();
        let result = evaluate(
            &green_bar_input(100.0, 101.0, 1_700_000_000),
            &SymbolFsm::default(),
            &cfg,
        );
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
        assert!(result.marker.is_none());
        assert!((result.new_fsm.prev_close - 101.0).abs() < f64::EPSILON);
    }

    #[test]
    fn idle_stays_idle_on_red_bar() {
        let cfg = SmartTrendConfig::default();
        let result = evaluate(
            &red_bar_input(100.0, 99.0, 1_700_000_000),
            &seeded_idle_fsm(100.0),
            &cfg,
        );
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
        assert_eq!(result.new_fsm.confirm_count, 0);
        assert!(result.marker.is_none());
        assert!((result.new_fsm.prev_close - 99.0).abs() < f64::EPSILON);
    }

    #[test]
    fn idle_to_confirming_on_green_bar() {
        // S36: this test covers the legacy "two-bar Idle → Confirming →
        // Active" progression. After S36, only 1m still requires two
        // bars (target=2); 5m and 10m skip Confirming entirely. Pin
        // this test to 1m so it remains meaningful.
        let cfg = SmartTrendConfig::default();
        let mut bar = green_bar_input(100.0, 101.0, 1_700_000_000);
        bar.timeframe = Timeframe::M1;
        let result = evaluate(&bar, &seeded_idle_fsm(100.0), &cfg);
        assert_eq!(result.new_fsm.phase, FsmPhase::Confirming);
        assert_eq!(result.new_fsm.confirm_count, 1);
        assert!(result.marker.is_none());
    }

    #[test]
    fn confirming_to_active_on_second_green_bar() {
        let cfg = SmartTrendConfig::default();
        let confirming_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            score: 60.0,
            confirm_count: 1,
            prev_close: 100.0,
            ..SymbolFsm::default()
        };
        let result = evaluate(
            &green_bar_input(100.0, 101.0, 1_700_000_300),
            &confirming_fsm,
            &cfg,
        );
        assert_eq!(result.new_fsm.phase, FsmPhase::Active);
        assert_eq!(result.new_fsm.confirm_count, 0);
        assert!(result.new_fsm.entry_price.is_some());
        assert!(result.new_fsm.entry_time.is_some());
        assert!(result.marker.as_ref().map_or(false, |m| m.kind == MarkerKind::Entry));
    }

    #[test]
    fn confirming_resets_on_red_bar() {
        let cfg = SmartTrendConfig::default();
        let confirming_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            score: 60.0,
            confirm_count: 1,
            prev_close: 100.0,
            ..SymbolFsm::default()
        };
        let mut input = red_bar_input(100.0, 99.0, 1_700_000_600);
        input.above_ema_slow = false;
        let result = evaluate(
            &input,
            &confirming_fsm,
            &cfg,
        );
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
        assert_eq!(result.new_fsm.confirm_count, 0);
        assert!(result.marker.is_none());
    }

    #[test]
    fn confirming_holds_on_stale_green_bar() {
        let cfg = SmartTrendConfig::default();
        let confirming_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            score: 60.0,
            confirm_count: 1,
            prev_close: 100.0,
            ..SymbolFsm::default()
        };
        let result = evaluate(
            &stale_green_input(100.0, 101.0, 1_700_000_600),
            &confirming_fsm,
            &cfg,
        );
        // Stale momentum no longer qualifies as entry -- holds Confirming
        assert_eq!(result.new_fsm.phase, FsmPhase::Confirming);
        assert!(result.marker.is_none());
    }

    #[test]
    fn active_tolerates_single_red_bar() {
        let cfg = SmartTrendConfig::default();
        let active_fsm = SymbolFsm {
            phase: FsmPhase::Active,
            score: 70.0,
            confirm_count: 0,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 101.0,
            ..SymbolFsm::default()
        };
        let result = evaluate(&red_bar_input(101.0, 100.0, 1_700_000_900), &active_fsm, &cfg);
        assert_eq!(result.new_fsm.phase, FsmPhase::Active);
        assert_eq!(result.new_fsm.confirm_count, 1);
        assert!(result.new_fsm.entry_price.is_some());
        assert!(result.marker.is_none());
    }

    #[test]
    fn active_exits_on_trailing_stop_5m() {
        let cfg = SmartTrendConfig::default();
        // 5m trailing = 0.6% from peak
        let active_fsm = SymbolFsm {
            phase: FsmPhase::Active,
            score: 70.0,
            confirm_count: 0,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 103.0,
            peak_price: 103.0,
            ..SymbolFsm::default()
        };
        // Price at 102.5: drop from 103 is 0.49% -- under 0.6% threshold, no trail
        let step1 = evaluate(&red_bar_input(103.0, 102.5, 1_700_000_900), &active_fsm, &cfg);
        assert_eq!(step1.new_fsm.phase, FsmPhase::Active);

        // Price at 102.3: drop from 103 is 0.68% -- above 0.6% -> trailing exit
        let step2 = evaluate(&red_bar_input(102.5, 102.3, 1_700_001_200), &step1.new_fsm, &cfg);
        assert_eq!(step2.new_fsm.phase, FsmPhase::Exit);
        assert!(step2.marker.iter().any(|m| m.kind == MarkerKind::Exit));
    }

    #[test]
    fn active_exits_after_red_bars_no_trail_5m() {
        let cfg = SmartTrendConfig::default();
        // Small drops within trailing tolerance (atr=0 => fallback 0.6%),
        // 3 consecutive reds trigger exit (exit_bars_for_tf 5m = base(2)+1 = 3)
        let active_fsm = SymbolFsm {
            phase: FsmPhase::Active,
            score: 70.0,
            confirm_count: 0,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 100.20,
            peak_price: 100.20,
            ..SymbolFsm::default()
        };
        let step1 = evaluate(&red_bar_input(100.20, 100.15, 1_700_000_900), &active_fsm, &cfg);
        assert_eq!(step1.new_fsm.phase, FsmPhase::Active);
        let step2 = evaluate(&red_bar_input(100.15, 100.10, 1_700_001_200), &step1.new_fsm, &cfg);
        assert_eq!(step2.new_fsm.phase, FsmPhase::Active);
        let step3 = evaluate(&red_bar_input(100.10, 100.05, 1_700_001_500), &step2.new_fsm, &cfg);
        assert_eq!(step3.new_fsm.phase, FsmPhase::Exit);
        assert!(step3.marker.iter().any(|m| m.kind == MarkerKind::Exit));
    }

    #[test]
    fn active_resets_red_count_on_green_bar() {
        let cfg = SmartTrendConfig::default();
        let active_with_one_red = SymbolFsm {
            phase: FsmPhase::Active,
            score: 70.0,
            confirm_count: 1,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 101.0,
            ..SymbolFsm::default()
        };
        let result = evaluate(
            &green_bar_input(101.0, 102.0, 1_700_001_500),
            &active_with_one_red,
            &cfg,
        );
        assert_eq!(result.new_fsm.phase, FsmPhase::Active);
        assert_eq!(result.new_fsm.confirm_count, 0);
        assert!(result.marker.is_none());
    }

    #[test]
    fn active_stays_on_green_bar() {
        let cfg = SmartTrendConfig::default();
        let active_fsm = SymbolFsm {
            phase: FsmPhase::Active,
            score: 70.0,
            confirm_count: 0,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 101.0,
            ..SymbolFsm::default()
        };
        let result = evaluate(
            &green_bar_input(101.0, 102.0, 1_700_001_200),
            &active_fsm,
            &cfg,
        );
        assert_eq!(result.new_fsm.phase, FsmPhase::Active);
        assert_eq!(result.new_fsm.entry_price, Some(100.0));
        assert!(result.marker.is_none());
    }

    #[test]
    fn stale_momentum_blocks_entry() {
        let cfg = SmartTrendConfig::default();
        let result = evaluate(
            &stale_green_input(100.0, 101.0, 1_700_001_500),
            &seeded_idle_fsm(100.0),
            &cfg,
        );
        assert!(result.diagnostics.momentum_score.stale);
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
        assert_eq!(result.new_fsm.confirm_count, 0);
    }

    #[test]
    fn full_cycle_entry_exit_cooldown_reentry() {
        // S36: pinned to 1m so the legacy two-bar progression still
        // applies (target=2 on 1m). The same scenario on 5m/10m is
        // covered by `s36_5m_transitions_active_in_single_bar`.
        // 1m parameters: cooldown=base(3)+3=6, exit=base(2), reentry=55+8=63
        //
        // S37 (2026-05-28): the timestamp is now explicitly anchored to
        // an RTH window (Tuesday 10:00 ET) so the new outside-RTH
        // relaxation in `cooldown_bars_for_tf` / `reentry_min_momentum`
        // does NOT trigger. The legacy cooldown/momentum semantics this
        // test exercises only apply during RTH; the AH/PM relaxation is
        // covered separately by `s37_outside_rth_collapses_cooldown_1m`.
        use chrono::TimeZone;
        let cfg = SmartTrendConfig::default();
        let t = chrono_tz::US::Eastern
            .with_ymd_and_hms(2026, 5, 12, 10, 0, 0)
            .unwrap()
            .timestamp();

        let on_1m = |inp: SmartTrendInput<'static>| -> SmartTrendInput<'static> {
            let mut x = inp;
            x.timeframe = Timeframe::M1;
            x
        };

        // 1-2. Idle -> Confirming -> Active (entry @ 102, peak=102)
        let step1 = evaluate(
            &on_1m(green_bar_input(100.0, 101.0, t)),
            &seeded_idle_fsm(100.0),
            &cfg,
        );
        assert_eq!(step1.new_fsm.phase, FsmPhase::Confirming);
        let step2 = evaluate(
            &on_1m(green_bar_input(101.0, 102.0, t + 60)),
            &step1.new_fsm,
            &cfg,
        );
        assert_eq!(step2.new_fsm.phase, FsmPhase::Active);
        assert!(step2.marker.iter().any(|m| m.kind == MarkerKind::Entry));
        assert_eq!(step2.new_fsm.peak_price, 102.0);

        // 3. Drop from 102 to 101.3 = 0.69% > 0.4% (1m trailing) -> Exit
        let step3 = evaluate(
            &on_1m(red_bar_input(102.0, 101.3, t + 120)),
            &step2.new_fsm,
            &cfg,
        );
        assert_eq!(step3.new_fsm.phase, FsmPhase::Exit);
        assert!(step3.marker.iter().any(|m| m.kind == MarkerKind::Exit));

        // 4-9. Six bars of cooldown (base(3)+3=6 for 1m)
        let mut prev = step3.new_fsm.clone();
        for j in 0..6 {
            let inp = on_1m(green_bar_input(101.0, 101.1, t + 180 + (j as i64) * 60));
            let r = evaluate(&inp, &prev, &cfg);
            assert_eq!(r.new_fsm.phase, FsmPhase::Exit, "cooldown bar {}", j + 1);
            prev = r.new_fsm;
        }
        assert_eq!(prev.cooldown_bars, 6);

        // 10. Cooldown expired + strong green: Exit -> Confirming
        let step_re = evaluate(
            &on_1m(green_bar_input(101.0, 102.0, t + 540)),
            &prev,
            &cfg,
        );
        assert_eq!(step_re.new_fsm.phase, FsmPhase::Confirming);

        // 11. Second green: Confirming -> Active (re-entry)
        let step_fin = evaluate(
            &on_1m(green_bar_input(102.0, 103.0, t + 600)),
            &step_re.new_fsm,
            &cfg,
        );
        assert_eq!(step_fin.new_fsm.phase, FsmPhase::Active);
        assert!(step_fin.marker.iter().any(|m| m.kind == MarkerKind::Entry));
    }

    #[test]
    fn legacy_stalking_normalizes_to_idle() {
        let cfg = SmartTrendConfig::default();
        let stalking_fsm = SymbolFsm {
            phase: FsmPhase::Stalking,
            score: 58.0,
            confirm_count: 7,
            entry_price: Some(123.0),
            entry_time: Some(1_700_000_000),
            prev_close: 100.0,
            ..SymbolFsm::default()
        };
        let result = evaluate(&red_bar_input(100.0, 99.0, 1_700_003_500), &stalking_fsm, &cfg);
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
        assert!(result.new_fsm.entry_price.is_none());
        assert!(result.new_fsm.entry_time.is_none());
    }

    #[test]
    fn exit_stays_during_cooldown() {
        let cfg = SmartTrendConfig::default();
        let exit_fsm = SymbolFsm {
            phase: FsmPhase::Exit,
            score: 40.0,
            confirm_count: 0,
            cooldown_bars: 0,
            entry_price: None,
            entry_time: None,
            prev_close: 100.0,
            ..SymbolFsm::default()
        };
        // Green bar during cooldown: stays in Exit, increments cooldown
        let result = evaluate(&green_bar_input(100.0, 101.0, 1_700_003_800), &exit_fsm, &cfg);
        assert_eq!(result.new_fsm.phase, FsmPhase::Exit);
        assert_eq!(result.new_fsm.cooldown_bars, 1);
        assert!(result.marker.is_none());

        // Red bar during cooldown: stays in Exit, increments cooldown
        let result2 = evaluate(&red_bar_input(101.0, 100.0, 1_700_004_100), &result.new_fsm, &cfg);
        assert_eq!(result2.new_fsm.phase, FsmPhase::Exit);
        assert_eq!(result2.new_fsm.cooldown_bars, 2);
    }

    #[test]
    fn exit_to_confirming_after_cooldown() {
        let cfg = SmartTrendConfig::default();
        // 5m cooldown = base(3) + 1 = 4
        let exit_fsm = SymbolFsm {
            phase: FsmPhase::Exit,
            score: 40.0,
            confirm_count: 0,
            cooldown_bars: 4,
            entry_price: None,
            entry_time: None,
            prev_close: 100.0,
            ..SymbolFsm::default()
        };
        let mut inp = green_bar_input(100.0, 101.0, 1_700_003_800);
        inp.timeframe = Timeframe::M15;
        let result = evaluate(&inp, &exit_fsm, &cfg);
        assert_eq!(result.new_fsm.phase, FsmPhase::Confirming);
        assert_eq!(result.new_fsm.confirm_count, 1);
        assert_eq!(result.new_fsm.cooldown_bars, 0);
        assert!(result.marker.is_none());
    }

    fn input_1m(
        prev_close: f64,
        current_price: f64,
        rvol: f64,
        roc: f64,
        bar_time: i64,
    ) -> SmartTrendInput<'static> {
        SmartTrendInput {
            momentum_input: MomentumInput { rvol, roc },
            current_price,
            prev_close,
            bar_time,
            timeframe: Timeframe::M1,
            symbol: "QBTS",
            atr: 0.0,
            regime: MarketRegime::Calm,
            macro_trend_bullish: true,
            above_ema_fast: true,
            above_ema_slow: true,
            news_sentiment: None,
        }
    }

    #[test]
    fn low_momentum_blocks_entry() {
        // S36.2: small TFs (1m/5m/10m) now use a relaxed momentum
        // floor (≤ 30) so this test pins to 1h where the original
        // strict default (52) still applies. A momentum-51 bar must
        // still be blocked on 1h+ where the strict floor is enforced.
        let cfg = SmartTrendConfig::default(); // entry_min_momentum=52
        let mut weak = input(100.0, 101.0, 0.5, 0.1, 1_700_000_000);
        weak.timeframe = Timeframe::H1;
        let result = evaluate(&weak, &seeded_idle_fsm(100.0), &cfg);
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
    }

    /// S36.2 — small TFs accept gradual climbs that produce
    /// individually-modest momentum scores (rvol=0.5, roc=0.1 ≈ 51)
    /// because the floor is 30 on 1m/5m/10m. Real-money entries are
    /// still gated by `bot.rs::min_entry_score` independently.
    #[test]
    fn s36_2_5m_accepts_modest_momentum() {
        let cfg = SmartTrendConfig::default(); // entry_min_momentum=52
        let mut weak = input(100.0, 101.0, 0.5, 0.1, 1_700_000_000);
        weak.timeframe = Timeframe::M5;
        let result = evaluate(&weak, &seeded_idle_fsm(100.0), &cfg);
        // 5m: target=1 → Idle → Active immediately (S36) once entry_ok
        // passes, which it does under the relaxed S36.2 floor (30).
        assert_eq!(result.new_fsm.phase, FsmPhase::Active);
        assert!(result
            .marker
            .iter()
            .any(|m| m.kind == MarkerKind::Entry));
    }

    #[test]
    fn high_momentum_allows_entry() {
        let cfg = SmartTrendConfig::default();
        // S36: pinned to 1m to preserve the legacy two-bar progression.
        // 5m / 10m promote Idle → Active in a single bar (target=1)
        // and would emit a marker here, which is covered by
        // `s36_5m_transitions_active_in_single_bar`.
        let mut strong = input(100.0, 101.0, 2.0, 1.5, 1_700_000_000);
        strong.timeframe = Timeframe::M1;
        let result = evaluate(&strong, &seeded_idle_fsm(100.0), &cfg);
        assert_eq!(result.new_fsm.phase, FsmPhase::Confirming);
        assert_eq!(result.new_fsm.confirm_count, 1);
    }

    #[test]
    fn trailing_stop_1m() {
        let cfg = SmartTrendConfig::default();
        // 1m trailing = 0.4% from peak
        let active = SymbolFsm {
            phase: FsmPhase::Active,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 100.50,
            peak_price: 100.50,
            ..SymbolFsm::default()
        };
        let t = 1_700_000_000i64;

        // Drop 0.3% from peak: no exit
        let s1 = evaluate(&input_1m(100.50, 100.20, 1.5, -2.0, t + 60), &active, &cfg);
        assert_eq!(s1.new_fsm.phase, FsmPhase::Active);

        // Drop 0.5% from peak (100.50 -> 100.0): trailing exit
        let s2 = evaluate(&input_1m(100.20, 100.0, 1.5, -2.0, t + 120), &s1.new_fsm, &cfg);
        assert_eq!(s2.new_fsm.phase, FsmPhase::Exit);
        assert!(s2.marker.iter().any(|m| m.kind == MarkerKind::Exit));
    }

    #[test]
    fn exit_bars_15m_needs_two() {
        let mut cfg = SmartTrendConfig::default();
        cfg.exit_confirm_bars = 2;
        let inp15 = |prev: f64, cur: f64, t: i64| SmartTrendInput {
            momentum_input: MomentumInput { rvol: 1.5, roc: -2.0 },
            current_price: cur,
            prev_close: prev,
            bar_time: t,
            timeframe: Timeframe::M15,
            symbol: "TEST",
            atr: 0.0,
            regime: MarketRegime::Calm,
            macro_trend_bullish: true,
            above_ema_fast: true,
            above_ema_slow: true,
            news_sentiment: None,
        };
        let active = SymbolFsm {
            phase: FsmPhase::Active,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 102.0,
            ..SymbolFsm::default()
        };
        let s1 = evaluate(&inp15(102.0, 101.0, 1_700_001_000), &active, &cfg);
        assert_eq!(s1.new_fsm.phase, FsmPhase::Active);
        let s2 = evaluate(&inp15(101.0, 100.0, 1_700_002_000), &s1.new_fsm, &cfg);
        assert_eq!(s2.new_fsm.phase, FsmPhase::Exit);
    }

    #[test]
    fn strength_classification() {
        assert_eq!(classify_strength(95.0), SmartTrendStrength::Extreme);
        assert_eq!(classify_strength(70.0), SmartTrendStrength::Strong);
        assert_eq!(classify_strength(58.0), SmartTrendStrength::Moderate);
        assert_eq!(classify_strength(53.0), SmartTrendStrength::Weak);
        assert_eq!(classify_strength(50.0), SmartTrendStrength::Weak);
    }

    #[test]
    fn diagnostics_serializable() {
        let cfg = SmartTrendConfig::default();
        let result = evaluate(
            &green_bar_input(100.0, 101.0, 1_700_004_100),
            &seeded_idle_fsm(100.0),
            &cfg,
        );
        let json = serde_json::to_string(&result.diagnostics).unwrap();
        assert!(json.contains("momentum_score"));
        assert!(json.contains("combined_score"));
        assert!(json.contains("strength"));
    }

    #[test]
    fn test_macro_consensus_gate_blocks_entry() {
        let cfg = SmartTrendConfig::default();
        let mut inp = green_bar_input(100.0, 102.0, 1_700_000_000);
        inp.macro_trend_bullish = false; // block it
        inp.timeframe = Timeframe::M1; // confirm target = 2

        let result = evaluate(&inp, &seeded_idle_fsm(100.0), &cfg);
        // Should stay Idle because macro consensus check fails
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
        assert!(result.marker.is_none());
    }

    #[test]
    fn test_pullback_transitions() {
        let cfg = SmartTrendConfig::default();

        // 1. Confirming state
        let confirming_fsm = SymbolFsm {
            phase: FsmPhase::Confirming,
            confirm_count: 1,
            prev_close: 102.0,
            ..SymbolFsm::default()
        };

        // Red bar but above EMA 21 -> transition to Pullback
        let mut red_above = red_bar_input(102.0, 101.5, 1_700_000_300);
        red_above.above_ema_slow = true;
        let r1 = evaluate(&red_above, &confirming_fsm, &cfg);
        assert_eq!(r1.new_fsm.phase, FsmPhase::Pullback);
        assert_eq!(r1.new_fsm.cooldown_bars, 0); // tracks pullback duration

        // Red bar and below EMA 21 -> transition back to Idle
        let mut red_below = red_bar_input(102.0, 101.0, 1_700_000_300);
        red_below.above_ema_slow = false;
        let r2 = evaluate(&red_below, &confirming_fsm, &cfg);
        assert_eq!(r2.new_fsm.phase, FsmPhase::Idle);

        // 2. Pullback state
        let pullback_fsm = SymbolFsm {
            phase: FsmPhase::Pullback,
            cooldown_bars: 2,
            prev_close: 101.5,
            ..SymbolFsm::default()
        };

        // Green bar with momentum (entry_ok) -> Reclaim! -> transition to Active
        let green_reclaim = green_bar_input(101.5, 103.0, 1_700_000_600);
        let r3 = evaluate(&green_reclaim, &pullback_fsm, &cfg);
        assert_eq!(r3.new_fsm.phase, FsmPhase::Active);
        assert!(r3.marker.iter().any(|m| m.kind == MarkerKind::Entry));

        // Below EMA 21 -> transition back to Idle
        let mut pullback_below = green_bar_input(101.5, 101.0, 1_700_000_600);
        pullback_below.above_ema_slow = false;
        let r4 = evaluate(&pullback_below, &pullback_fsm, &cfg);
        assert_eq!(r4.new_fsm.phase, FsmPhase::Idle);

        // Pulled back for 5 bars -> Timeout -> transition back to Idle
        let pullback_expired_fsm = SymbolFsm {
            phase: FsmPhase::Pullback,
            cooldown_bars: 4, // next step makes it 5
            prev_close: 101.5,
            ..SymbolFsm::default()
        };
        let mut green_no_reclaim = stale_green_input(101.5, 101.8, 1_700_000_600);
        green_no_reclaim.above_ema_slow = true;
        // momentum score will be low since roc is small/zero
        let r5 = evaluate(&green_no_reclaim, &pullback_expired_fsm, &cfg);
        assert_eq!(r5.new_fsm.phase, FsmPhase::Idle);
    }

    #[test]
    fn test_regime_adaptive_chandelier_exit() {
        let cfg = SmartTrendConfig::default();

        let active_fsm = SymbolFsm {
            phase: FsmPhase::Active,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 105.0,
            peak_price: 105.0,
            ..SymbolFsm::default()
        };

        // ATR = 1.0
        // Trending regime: multiplier is 2.0 -> stop at 105.0 - 2.0 = 103.0
        let mut trending_input = red_bar_input(105.0, 103.5, 1_700_000_300);
        trending_input.regime = MarketRegime::Trending;
        trending_input.atr = 1.0;
        
        let r_trend_hold = evaluate(&trending_input, &active_fsm, &cfg);
        assert_eq!(r_trend_hold.new_fsm.phase, FsmPhase::Active); // 103.5 > 103.0 -> Hold

        trending_input.current_price = 102.5;
        let r_trend_exit = evaluate(&trending_input, &active_fsm, &cfg);
        assert_eq!(r_trend_exit.new_fsm.phase, FsmPhase::Exit); // 102.5 < 103.0 -> Exit

        // Volatile regime: multiplier is 2.8 -> stop at 105.0 - 2.8 = 102.2
        let mut volatile_input = red_bar_input(105.0, 102.5, 1_700_000_300);
        volatile_input.regime = MarketRegime::Volatile;
        volatile_input.atr = 1.0;

        let r_vol_hold = evaluate(&volatile_input, &active_fsm, &cfg);
        assert_eq!(r_vol_hold.new_fsm.phase, FsmPhase::Active); // 102.5 > 102.2 -> Hold

        volatile_input.current_price = 101.5;
        let r_vol_exit = evaluate(&volatile_input, &active_fsm, &cfg);
        assert_eq!(r_vol_exit.new_fsm.phase, FsmPhase::Exit); // 101.5 < 102.2 -> Exit
    }

    #[test]
    fn test_climax_volume_guard() {
        let cfg = SmartTrendConfig::default();

        // Extremely high volume (rvol = 4.0) on positive ROC (2.0)
        let mut climax_input = green_bar_input(100.0, 101.0, 1_700_000_000);
        climax_input.momentum_input.rvol = 4.0;
        climax_input.momentum_input.roc = 2.0;

        let result = evaluate(&climax_input, &seeded_idle_fsm(100.0), &cfg);
        // Diagnostics score should be capped at 52.0 (neutral) and FSM stays Idle
        assert!(result.diagnostics.momentum_score.score <= 52.0);
        assert_eq!(result.new_fsm.phase, FsmPhase::Idle);
    }

    #[test]
    fn test_climax_profit_taking_active_tightens() {
        let cfg = SmartTrendConfig::default();

        let active_fsm = SymbolFsm {
            phase: FsmPhase::Active,
            entry_price: Some(100.0),
            entry_time: Some(1_700_000_000),
            prev_close: 100.0,
            peak_price: 100.0,
            cooldown_bars: 0, // climax_occurred is false
            ..SymbolFsm::default()
        };

        // Climax volume bar: green bar with rvol = 4.0, roc = 2.0.
        // Even though rvol > 3.0 caps momentum score to 25.0, the FSM stays Active (since it's a green bar).
        let mut climax_input = green_bar_input(100.0, 101.0, 1_700_000_100);
        climax_input.momentum_input.rvol = 4.0;
        climax_input.momentum_input.roc = 2.0;
        climax_input.atr = 1.0;
        climax_input.regime = MarketRegime::Trending;

        let r1 = evaluate(&climax_input, &active_fsm, &cfg);
        assert_eq!(r1.new_fsm.phase, FsmPhase::Active);
        assert_eq!(r1.new_fsm.peak_price, 101.0);
        assert_eq!(r1.new_fsm.cooldown_bars, 1); // Climax state persisted!

        // Subsequent minor pullback: price drops from peak 101.0 to 100.4.
        // ATR = 1.0. Chandelier stop: peak_price - atr * K.
        // With climax flag active, K = 0.5 -> stop = 101.0 - 0.5 = 100.5.
        // Since 100.4 < 100.5, this triggers exit.
        let mut minor_pullback_input = red_bar_input(101.0, 100.4, 1_700_000_200);
        minor_pullback_input.atr = 1.0;
        minor_pullback_input.regime = MarketRegime::Trending;

        let r2 = evaluate(&minor_pullback_input, &r1.new_fsm, &cfg);
        assert_eq!(r2.new_fsm.phase, FsmPhase::Exit);
        assert!(r2.marker.iter().any(|m| m.kind == MarkerKind::Exit));
    }

    #[test]
    fn test_pullback_time_decay_reclaim() {
        let mut cfg = SmartTrendConfig::default();
        cfg.entry_min_momentum = 60.0; // floor on 15m is 45.0

        // Start in Pullback state at bar 0
        let pullback_fsm = SymbolFsm {
            phase: FsmPhase::Pullback,
            cooldown_bars: 0,
            prev_close: 100.0,
            ..SymbolFsm::default()
        };

        // Green bar with momentum score = 41.0 (is_green_bar is true, but roc is negative mock)
        let reclaim_input = SmartTrendInput {
            momentum_input: MomentumInput { rvol: 1.0, roc: -0.45 },
            current_price: 101.0,
            prev_close: 100.0,
            bar_time: 1_700_000_100,
            timeframe: Timeframe::M15,
            symbol: "TEST",
            atr: 1.0,
            regime: MarketRegime::Trending,
            macro_trend_bullish: true,
            above_ema_fast: true,
            above_ema_slow: true,
            news_sentiment: None,
        };

        // First pullback bar check: cooldown_bars becomes 1.
        // Decay factor: 1.0 -> Min momentum threshold: 45.0.
        // Score (41.0) < 45.0 -> Stay in Pullback.
        let r1 = evaluate(&reclaim_input, &pullback_fsm, &cfg);
        assert_eq!(r1.new_fsm.phase, FsmPhase::Pullback);
        assert_eq!(r1.new_fsm.cooldown_bars, 1);

        // Update FSM state to simulate 2 bars spent in pullback
        let mut pullback_fsm_bar2 = r1.new_fsm.clone();
        pullback_fsm_bar2.cooldown_bars = 2;
        pullback_fsm_bar2.prev_close = 100.0;

        // Third pullback bar check: cooldown_bars increments to 3.
        // Decay factor: 1.0 - 0.1 * 2 = 0.8 -> Min momentum threshold: 45.0 * 0.8 = 36.0.
        // Score (41.0) >= 36.0 -> Reclaim successful! FSM transitions to Active.
        let r2 = evaluate(&reclaim_input, &pullback_fsm_bar2, &cfg);
        assert_eq!(r2.new_fsm.phase, FsmPhase::Active);
        assert_eq!(r2.new_fsm.cooldown_bars, 0);
        assert!(r2.marker.iter().any(|m| m.kind == MarkerKind::Entry));
    }
}

