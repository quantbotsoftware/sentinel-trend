//! Pure replay: history (+ today's partial session) → the validated engine
//! → desired net shares per symbol and today's fills ("intents").
//!
//! Deterministic by construction: every poll re-runs the full simulation
//! from the cached history with fresh sleeves, so live state can never
//! drift from what the backtest would have done on the same bars.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde::Serialize;
use titan_meridian::data::load_jsonl_days_live;
use titan_meridian::envelope::NoiseEnvelope;
use titan_meridian::vol::{self, VolWindows};
use titan_meridian::vwap::Vwap;
use titan_meridian::sim::{run, SimConfig, SymbolData, TradeRecord};
use titan_meridian::sleeve::Sleeve;
use titan_meridian::sleeves::{IntradayMomentum, IntradayParams, OvernightCarry, OvernightParams};

use super::config::MeridianConfig;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Intent {
    pub symbol: String,
    pub bar_idx: usize,
    /// Signed shares (+ buy / − sell).
    pub delta_shares: f64,
    pub fill_price: f64,
    pub sleeves: Vec<String>,
    /// Bar start, unix seconds (chart marker anchor).
    pub bar_time: i64,
    /// "HH:MM" ET of the bar.
    pub time_et: String,
    /// Net shares in the symbol after this fill.
    pub net_after: f64,
    /// Operator-facing rule explanation (what a human would check on the chart).
    pub label: String,
}

/// Start of RTH bar `idx` on `date` (09:30 ET + idx minutes) as UTC seconds.
pub fn bar_start_utc(date: NaiveDate, idx: usize) -> Option<i64> {
    use chrono::TimeZone;
    let t = date.and_hms_opt(9, 30, 0)? + chrono::Duration::minutes(idx as i64);
    chrono_tz::America::New_York.from_local_datetime(&t).single().map(|d| d.timestamp())
}

fn label_for(sleeves: &[String], idx: usize, delta: f64, net_after: f64, session_bars: usize) -> String {
    let has = |s: &str| sleeves.iter().any(|x| x == s);
    let last = idx + 1 >= session_bars;
    if has("B") && idx == 0 {
        return "B · sell the overnight carry at the 09:30 open".into();
    }
    if has("B") && last {
        return "B · overnight carry: buy at the close (IBS low, above SMA200), sell at tomorrow's open".into();
    }
    if has("A") && last {
        return "A · flat at the close — intraday sleeve never holds overnight".into();
    }
    if has("A") {
        return match (delta > 0.0, net_after) {
            (true, n) if n > 0.0 => "A · LONG: 30-min close above the upper noise band — trail stop = VWAP / band".into(),
            (false, n) if n < 0.0 => "A · SHORT: 30-min close below the lower noise band — trail stop = VWAP / band".into(),
            _ => "A · EXIT: close crossed back through VWAP / band — position closed".into(),
        };
    }
    "netting adjustment".into()
}

/// What a discretionary trader needs to verify the rules on the chart.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct SymbolView {
    pub symbol: String,
    /// Latest bar close today (None before the first bar).
    pub last: Option<f64>,
    pub open: Option<f64>,
    pub prev_close: f64,
    /// Sleeve A noise bands at the latest bar (upper, lower).
    pub upper: Option<f64>,
    pub lower: Option<f64>,
    pub vwap: Option<f64>,
    /// Sleeve B inputs: where today closes in its range (0 = low, 1 = high),
    /// the 200-day SMA and whether the last close is above it.
    pub ibs: Option<f64>,
    pub sma200: Option<f64>,
    pub above_sma: Option<bool>,
    /// Daily σ used for sizing (close-to-close, 14 d) and overnight σ (20 d).
    pub sigma_cc: Option<f64>,
    pub sigma_on: Option<f64>,
    pub net_shares: f64,
}

pub fn symbol_view(cfg: &MeridianConfig, s: &SymbolData, today: Option<NaiveDate>, net_shares: f64) -> SymbolView {
    let mut v = SymbolView { symbol: s.symbol.clone(), net_shares, ..Default::default() };
    let Some(last_day) = s.days.last() else { return v };
    let is_today = Some(last_day.date) == today;
    let hist: &[titan_meridian::DaySeries] = if is_today { &s.days[..s.days.len() - 1] } else { &s.days };
    let Some(prev) = hist.last() else { return v };
    v.prev_close = prev.close();
    let vs = vol::compute(hist, VolWindows { cc: 14, on: 20, atr: 14, sma: 200 });
    v.sma200 = vs.sma;
    v.sigma_cc = vs.sigma_cc;
    v.sigma_on = vs.sigma_on;
    let ref_close = if is_today { last_day.close() } else { prev.close() };
    v.above_sma = vs.sma.map(|m| ref_close > m);
    if !is_today {
        return v;
    }
    let idx = last_day.len().saturating_sub(1);
    v.last = Some(last_day.close());
    v.open = Some(last_day.open());
    v.ibs = Some(last_day.ibs());
    let mut vw = Vwap::new();
    for b in &last_day.bars {
        vw.push(b);
    }
    v.vwap = Some(vw.value());
    if let Some(env) = NoiseEnvelope::fit(hist, cfg.envelope_lookback, cfg.envelope_mult) {
        let (u, l) = env.bands(idx, last_day.open(), prev.close());
        v.upper = Some(u);
        v.lower = Some(l);
    }
    v
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReplaySnapshot {
    pub date: Option<NaiveDate>,
    /// Bars seen today per symbol.
    pub bars_today: BTreeMap<String, usize>,
    pub sessions: BTreeMap<String, usize>,
    /// Desired net shares per symbol after the latest bar.
    pub net_shares: BTreeMap<String, f64>,
    /// Fills the engine produced today, in order.
    pub intents_today: Vec<Intent>,
    /// Equity of the shadow book (engine equity, marks at last close).
    pub equity: f64,
    pub total_cost: f64,
    /// Per-symbol chart view (bands, VWAP, IBS, SMA200) at the latest bar.
    pub views: Vec<SymbolView>,
    /// Per-symbol split of `net_shares` by sleeve ("A" flat at close, "B" overnight).
    pub sleeve_shares: BTreeMap<String, BTreeMap<String, f64>>,
}

pub fn sim_config(cfg: &MeridianConfig, live_session_bars: Option<usize>) -> SimConfig {
    SimConfig {
        initial_equity: cfg.capital,
        slippage_per_share: cfg.slippage_per_share,
        envelope_lookback: cfg.envelope_lookback,
        envelope_mult: cfg.envelope_mult,
        warmup_days: cfg.envelope_lookback + 14,
        live_session_bars,
        risk: cfg.risk_overlay.then(titan_meridian::risk::RiskParams::default),
        // phase 4b: size on the configured capital, not on the equity the
        // replay compounded over the history cache (was $114.7K vs $98.3K real)
        fixed_sizing_equity: Some(cfg.capital),
        ..Default::default()
    }
}

pub fn make_sleeves(cfg: &MeridianConfig) -> Vec<Box<dyn Sleeve>> {
    // the envelope is fitted with `envelope_mult`; each ensemble member
    // scales its σ by `band_mult` on top (single member ⇒ 1.0 = no change)
    let a = IntradayParams {
        lookback_days: cfg.envelope_lookback,
        band_mult: 1.0,
        sigma_target_daily: cfg.sigma_target_daily,
        max_leverage: cfg.max_leverage,
        allow_short: cfg.allow_short,
    };
    let b = OvernightParams {
        sigma_target_night: cfg.sigma_target_night,
        ibs_cut: cfg.ibs_cut,
        decide_bars_before_close: cfg.b_decide_offset,
        ..Default::default()
    };
    let mut sleeves: Vec<Box<dyn Sleeve>> = Vec::new();
    for m in IntradayMomentum::ensemble(a, &cfg.band_mults) {
        sleeves.push(Box::new(m));
    }
    sleeves.push(Box::new(OvernightCarry::new(b)));
    sleeves
}

fn to_intent(t: &TradeRecord, net_after: f64, session_bars: usize) -> Intent {
    let sleeves: Vec<String> = t.sleeves.iter().map(|s| s.as_str().to_string()).collect();
    let label = label_for(&sleeves, t.bar_idx, t.delta_shares, net_after, session_bars);
    let m = 9 * 60 + 30 + t.bar_idx;
    Intent {
        symbol: t.symbol.clone(),
        bar_idx: t.bar_idx,
        delta_shares: t.delta_shares,
        fill_price: t.fill_price,
        sleeves,
        bar_time: bar_start_utc(t.date, t.bar_idx).unwrap_or(0),
        time_et: format!("{:02}:{:02}", m / 60, m % 60),
        net_after,
        label,
    }
}

/// Replay the engine over `data`. `today` marks the partial session whose
/// scheduled length is `session_bars` (390, or 210 on an early close).
pub fn replay(cfg: &MeridianConfig, data: &[SymbolData], today: Option<NaiveDate>, session_bars: usize) -> ReplaySnapshot {
    replay_inner(cfg, data, today, session_bars, false)
}

/// Close preview (15:48 ET): the last available bar is treated as the
/// closing bar, so the close decisions (A flat, B carry) appear now. Their
/// `bar_idx` is reported as the closing bar so the 16:00 replay de-dups.
pub fn replay_close_preview(cfg: &MeridianConfig, data: &[SymbolData], today: NaiveDate, session_bars: usize) -> ReplaySnapshot {
    let mut snap = replay_inner(cfg, data, Some(today), session_bars, true);
    for i in snap.intents_today.iter_mut() {
        if i.bar_idx + 1 >= session_bars - cfg.b_decide_offset.max(1) {
            i.bar_idx = session_bars - 1;
            let m = 9 * 60 + 30 + i.bar_idx;
            i.time_et = format!("{:02}:{:02}", m / 60, m % 60);
        }
    }
    snap
}

fn replay_inner(cfg: &MeridianConfig, data: &[SymbolData], today: Option<NaiveDate>, session_bars: usize, force_close: bool) -> ReplaySnapshot {
    let mut sim = sim_config(cfg, today.map(|_| session_bars));
    sim.live_force_close = force_close;
    let res = run(&sim, data, &|| make_sleeves(cfg));
    let mut snap = ReplaySnapshot { date: today, equity: res.equity.last().copied().unwrap_or(cfg.capital), total_cost: res.total_cost, ..Default::default() };
    for s in data {
        snap.sessions.insert(s.symbol.clone(), s.days.len());
        let today_bars = s.days.last().filter(|d| Some(d.date) == today).map(|d| d.len()).unwrap_or(0);
        snap.bars_today.insert(s.symbol.clone(), today_bars);
        snap.net_shares.insert(s.symbol.clone(), 0.0);
    }
    for t in &res.trades {
        let net = snap.net_shares.entry(t.symbol.clone()).or_default();
        *net += t.delta_shares;
        let net_after = *net;
        if Some(t.date) == today {
            snap.intents_today.push(to_intent(t, net_after, session_bars));
        }
    }
    snap.views = data.iter().map(|s| symbol_view(cfg, s, today, snap.net_shares.get(&s.symbol).copied().unwrap_or(0.0))).collect();
    snap.sleeve_shares = res
        .sleeve_shares
        .iter()
        .map(|(sym, m)| (sym.clone(), m.iter().map(|(id, q)| (id.as_str().to_string(), *q)).collect()))
        .collect();
    snap
}

/// Load the per-symbol caches into engine input. Symbols whose cache is
/// missing or too short are reported in the error list and skipped.
pub fn load_data(cfg: &MeridianConfig, today: Option<NaiveDate>) -> (Vec<SymbolData>, Vec<String>) {
    let mut data = Vec::new();
    let mut errors = Vec::new();
    let min_sessions = cfg.envelope_lookback + 15;
    for sym in &cfg.symbols {
        let path = super::history::cache_path(sym);
        match load_jsonl_days_live(&path, 300, today) {
            Ok((days, _)) if days.len() >= min_sessions => data.push(SymbolData { symbol: sym.to_uppercase(), days }),
            Ok((days, _)) => errors.push(format!("{sym}: {} sessions cached, need ≥ {min_sessions}", days.len())),
            Err(e) => errors.push(format!("{sym}: {e}")),
        }
    }
    (data, errors)
}

/// Identity of an intent for de-duplication across polls and restarts.
pub fn intent_key(i: &Intent) -> (String, usize, i64) {
    (i.symbol.clone(), i.bar_idx, (i.delta_shares * 1000.0).round() as i64)
}

/// Intents in `now` that were not in `prev` (same symbol, bar, delta).
pub fn new_intents(prev: &ReplaySnapshot, now: &ReplaySnapshot) -> Vec<Intent> {
    if prev.date != now.date {
        return now.intents_today.clone();
    }
    let seen: std::collections::HashSet<_> = prev.intents_today.iter().map(intent_key).collect();
    now.intents_today.iter().filter(|i| !seen.contains(&intent_key(i))).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use titan_meridian::bars::{Bar, DaySeries};
    use titan_meridian::session::FULL_SESSION_BARS;

    fn day(n: u32, o: f64, c: f64, bars: usize) -> DaySeries {
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap_or_default() + chrono::Days::new(n as u64);
        let b = (0..bars).map(|m| {
            let p = o + (c - o) * m as f64 / 389.0;
            Bar { o: p, h: p, l: p, c: p, v: 100.0 }
        });
        DaySeries { date, bars: b.collect(), real_bars: bars }
    }

    fn cfg() -> MeridianConfig {
        MeridianConfig { sigma_target_night: 1.0, risk_overlay: false, b_decide_offset: 0, ..Default::default() }
    }

    #[test]
    fn replay_is_deterministic_and_partial_today_holds_close_logic() {
        // 40 warm-up days (IBS 1 ⇒ no carry), then a day closing at its low.
        let mut days: Vec<DaySeries> = (0..40).map(|i| day(i, 100.0, 100.1 + (i % 3) as f64 * 0.1, 390)).collect();
        days.push(day(40, 101.0, 100.0, 390));
        let today = days.last().map(|d| d.date);
        let full = vec![SymbolData { symbol: "SPY".into(), days: days.clone() }];
        // complete day, historical mode ⇒ carry entered at the close
        let s1 = replay(&cfg(), &full, None, FULL_SESSION_BARS);
        let s2 = replay(&cfg(), &full, None, FULL_SESSION_BARS);
        assert_eq!(s1.net_shares["SPY"], s2.net_shares["SPY"], "deterministic");
        assert!(s1.net_shares["SPY"] > 0.0, "carry position after the close: {s1:?}");
        assert_eq!(s1.sleeve_shares["SPY"]["B"], s1.net_shares["SPY"], "the whole position belongs to B");
        // same day still in progress (200 bars) ⇒ no close entry yet
        let mut partial = days.clone();
        if let Some(d) = partial.last_mut() {
            d.bars.truncate(200);
            d.real_bars = 200;
        }
        let s3 = replay(&cfg(), &[SymbolData { symbol: "SPY".into(), days: partial }], today, FULL_SESSION_BARS);
        assert_eq!(s3.net_shares["SPY"], 0.0);
        assert_eq!(s3.bars_today["SPY"], 200);
        assert!(s3.intents_today.is_empty());
    }

    #[test]
    fn new_intents_diffs_by_content_and_resets_on_new_day() {
        let i = |bar: usize| Intent {
            symbol: "SPY".into(),
            bar_idx: bar,
            delta_shares: 10.0,
            fill_price: 1.0,
            sleeves: vec!["A".into()],
            bar_time: 0,
            time_et: String::new(),
            net_after: 10.0,
            label: String::new(),
        };
        let d = NaiveDate::from_ymd_opt(2026, 9, 3);
        let prev = ReplaySnapshot { date: d, intents_today: vec![i(29)], ..Default::default() };
        let now = ReplaySnapshot { date: d, intents_today: vec![i(29), i(59)], ..Default::default() };
        assert_eq!(new_intents(&prev, &now), vec![i(59)]);
        let tomorrow = ReplaySnapshot { date: NaiveDate::from_ymd_opt(2026, 9, 4), intents_today: vec![i(29)], ..Default::default() };
        assert_eq!(new_intents(&now, &tomorrow), vec![i(29)]);
    }

    #[test]
    fn labels_and_bar_times_are_human_readable() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 3).unwrap_or_default(); // EDT
        assert_eq!(bar_start_utc(d, 0), Some(1_788_442_200)); // 2026-09-03 13:30:00Z
        assert_eq!(bar_start_utc(d, 60), Some(1_788_442_200 + 3600));
        let a = vec!["A".to_string()];
        let b = vec!["B".to_string()];
        assert!(label_for(&a, 29, 300.0, 300.0, 390).starts_with("A · LONG"));
        assert!(label_for(&a, 59, -300.0, 0.0, 390).starts_with("A · EXIT"));
        assert!(label_for(&a, 59, -300.0, -300.0, 390).starts_with("A · SHORT"));
        assert!(label_for(&a, 389, -300.0, 0.0, 390).starts_with("A · flat"));
        assert!(label_for(&a, 209, -300.0, 0.0, 210).starts_with("A · flat"), "early close: bar 209 is the close");
        assert!(label_for(&b, 389, 150.0, 150.0, 390).starts_with("B · overnight carry"));
        assert!(label_for(&b, 0, -150.0, 0.0, 390).starts_with("B · sell"));
        let t = TradeRecord { date: d, symbol: "SPY".into(), bar_idx: 30, delta_shares: 1.0, fill_price: 1.0, cost: 0.0, sleeves: vec![titan_meridian::SleeveId::A] };
        assert_eq!(to_intent(&t, 1.0, 390).time_et, "10:00");
    }

    #[test]
    fn symbol_view_exposes_bands_vwap_ibs_and_sma() {
        let mut days: Vec<DaySeries> = (0..40).map(|i| day(i, 100.0, 100.1 + (i % 3) as f64 * 0.1, 390)).collect();
        days.push(day(40, 101.0, 100.0, 200)); // today, partial, falling
        let today = days.last().map(|d| d.date);
        let sd = SymbolData { symbol: "SPY".into(), days };
        let v = symbol_view(&cfg(), &sd, today, 0.0);
        assert_eq!(v.open, Some(101.0));
        assert!((v.prev_close - 100.1).abs() < 1e-9, "day 39 closes at 100.1: {v:?}");
        assert!(v.ibs.is_some_and(|x| x < 0.01), "closing at the low so far ⇒ IBS ≈ 0");
        let (u, l) = (v.upper.unwrap_or(0.0), v.lower.unwrap_or(0.0));
        assert!(u >= 101.0 && l <= 100.1, "bands bracket max(open, prev) / min(open, prev): {u} {l}");
        assert!(v.vwap.is_some_and(|w| w > 100.0 && w < 101.0));
        assert!(v.sma200.is_none(), "only 40 sessions ⇒ SMA200 unavailable, not fabricated");
        // before today's first bar: chart values absent, history stats present
        let sd2 = SymbolData { symbol: "SPY".into(), days: sd.days[..40].to_vec() };
        let v2 = symbol_view(&cfg(), &sd2, today, 0.0);
        assert!(v2.last.is_none() && v2.upper.is_none() && v2.sigma_cc.is_some());
    }

    #[test]
    fn load_data_reports_missing_cache() {
        let c = MeridianConfig { symbols: vec!["ZZZZNOPE".into()], ..Default::default() };
        let (data, errors) = load_data(&c, None);
        assert!(data.is_empty());
        assert_eq!(errors.len(), 1);
    }
}
