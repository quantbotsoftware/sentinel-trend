//! Minute-by-minute historical simulation of sleeves + netting + risk.
//!
//! Fills: at the bar's open or close (per target) ± `slippage_per_share`
//! against the trade direction, plus `commission_per_share`. One net delta
//! per symbol per fill point — sleeves that offset each other cost nothing
//! (the point of netting). Deterministic: no randomness anywhere.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::bars::DaySeries;
use crate::envelope::NoiseEnvelope;
use crate::netting::{delta_shares, NettingParams, SymbolBook};
use crate::risk::{RiskOverlay, RiskParams, RiskState};
use crate::sleeve::{DayCtx, FillAt, Sleeve, SleeveId, Target};
use crate::vol::{self, VolWindows};
use crate::vwap::Vwap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SimConfig {
    pub initial_equity: f64,
    pub slippage_per_share: f64,
    pub commission_per_share: f64,
    pub envelope_lookback: usize,
    pub envelope_mult: f64,
    pub vol_cc_window: usize,
    pub vol_on_window: usize,
    pub sma_window: usize,
    /// Days skipped before the first trade (indicator warm-up).
    pub warmup_days: usize,
    /// Live replay: scheduled length of the *last* day in the data when it
    /// is still in progress (390 full / 210 half). `None` = historical
    /// simulation, every day is complete.
    pub live_session_bars: Option<usize>,
    /// Live close preview: on the last day, the last available bar is also
    /// treated as the closing bar (see `DayCtx::force_close`).
    pub live_force_close: bool,
    /// Size every day on this fixed equity instead of the compounding
    /// simulated equity (phase 4b: capital fixed; compounding is a phase-5
    /// decision). `None` = compound (research default).
    pub fixed_sizing_equity: Option<f64>,
    /// Execution-latency stress: intraday (non-auction) fills happen at the
    /// close of the bar this many bars *after* the decision bar. 0 = fill
    /// at the decision bar's close (research default; live measured ≈ 20 s).
    pub fill_delay_bars: usize,
    /// On a fresh daily-loss halt, flatten the intraday sleeve at that bar
    /// (plan §3.8.3). `false` = ablation: the halt only blocks new exposure
    /// and A exits by its own rules (trail / close).
    pub halt_flatten_intraday: bool,
    pub netting: Option<NettingParams>,
    pub risk: Option<RiskParams>,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            initial_equity: 100_000.0,
            slippage_per_share: 0.01,
            commission_per_share: 0.0,
            envelope_lookback: 14,
            envelope_mult: 1.0,
            vol_cc_window: 14,
            vol_on_window: 20,
            sma_window: 200,
            warmup_days: 28,
            live_session_bars: None,
            live_force_close: false,
            fixed_sizing_equity: None,
            fill_delay_bars: 0,
            halt_flatten_intraday: true,
            netting: None,
            risk: None,
        }
    }
}

pub struct SymbolData {
    pub symbol: String,
    /// Chronological, one entry per usable session.
    pub days: Vec<DaySeries>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub date: NaiveDate,
    pub symbol: String,
    pub bar_idx: usize,
    pub delta_shares: f64,
    pub fill_price: f64,
    pub cost: f64,
    pub sleeves: Vec<SleeveId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimResult {
    pub dates: Vec<NaiveDate>,
    pub equity: Vec<f64>,
    pub daily_returns: Vec<f64>,
    /// Gross (pre-cost) $ P/L per sleeve per day, marked on the sleeve's own shares.
    pub sleeve_pnl: BTreeMap<SleeveId, Vec<f64>>,
    pub trades: Vec<TradeRecord>,
    pub total_cost: f64,
    pub risk_halts: u32,
    /// Per-symbol, per-sleeve share attribution after the last processed
    /// bar (live: which part of the net position belongs to A — must be
    /// flat at the close — and which to B — carried overnight).
    pub sleeve_shares: BTreeMap<String, BTreeMap<SleeveId, f64>>,
}

#[derive(Default)]
struct SleeveLedger {
    shares: f64,
    mark: f64,
    pnl_today: f64,
}

struct SymState {
    book: SymbolBook,
    net_shares: f64,
    ledgers: BTreeMap<SleeveId, SleeveLedger>,
}

fn fill_price(px: f64, delta: f64, slip: f64) -> f64 {
    if delta > 0.0 {
        px + slip
    } else {
        px - slip
    }
}

/// Bring the symbol's net position to the book's desired net at `px`:
/// mark the touched sleeves' attribution, then trade the (netted) delta.
/// `touched` may repeat a sleeve (several ensemble members on one bar);
/// attribution is per sleeve, so it is de-duplicated here.
#[allow(clippy::too_many_arguments)]
fn fill(cash: &mut f64, st: &mut SymState, res: &mut SimResult, cfg: &SimConfig, ctx: &DayCtx, symbol: &str, idx: usize, px: f64, touched: &[SleeveId]) {
    let per_sleeve = st.book.sleeve_shares(ctx.equity);
    let mut touched: Vec<SleeveId> = touched.to_vec();
    touched.sort();
    touched.dedup();
    for id in &touched {
        let l = st.ledgers.entry(*id).or_default();
        l.pnl_today += l.shares * (px - l.mark);
        l.mark = px;
        l.shares = per_sleeve.get(id).copied().unwrap_or(0.0);
    }
    let desired = st.book.net_shares(ctx.equity);
    let delta = match cfg.netting.as_ref() {
        Some(np) => delta_shares(st.net_shares, desired, px, np),
        None => (desired != st.net_shares).then_some(desired - st.net_shares),
    };
    if let Some(d) = delta {
        let fp = fill_price(px, d, cfg.slippage_per_share);
        let cost = d.abs() * (cfg.slippage_per_share + cfg.commission_per_share);
        *cash -= d * fp + d.abs() * cfg.commission_per_share;
        st.net_shares += d;
        res.total_cost += cost;
        res.trades.push(TradeRecord {
            date: ctx.date,
            symbol: symbol.to_string(),
            bar_idx: idx,
            delta_shares: d,
            fill_price: fp,
            cost,
            sleeves: touched,
        });
    }
}

/// Per-symbol working set for one session (built before the minute loop).
struct DayRun<'a> {
    si: usize,
    today: &'a DaySeries,
    ctx: DayCtx<'a>,
    vwap: Vwap,
    pending: Vec<(usize, Vec<SleeveId>)>,
    /// Latest bar close seen (portfolio mark while other symbols trade).
    mark_px: f64,
}

/// Run all sleeves over all symbols. `make_sleeves` is called once per
/// symbol — sleeve state (positions, flags) is per symbol, never shared.
/// Calendar = union of dates; a symbol without a session on a date is
/// simply skipped that day.
///
/// The minute loop is **time-synchronous across symbols** (bar `idx` of
/// every symbol, then `idx + 1`), so the risk overlay sees the whole
/// portfolio at each minute — as the live rail does. (Until 2026-09-18 the
/// engine ran SPY's whole day, then QQQ's: a SPY halt at 10:30 blocked QQQ
/// from 09:30, a QQQ halt never reached SPY.)
pub fn run(cfg: &SimConfig, data: &[SymbolData], make_sleeves: &dyn Fn() -> Vec<Box<dyn Sleeve>>) -> SimResult {
    run_with(cfg, data, &|_| make_sleeves())
}

/// Like [`run`], but the factory receives the symbol, so a portfolio can
/// carry different sleeve sets per instrument (e.g. A+B on SPY/QQQ, A only
/// on a diversifier whose overnight premium is not part of the design).
pub fn run_with(cfg: &SimConfig, data: &[SymbolData], make_sleeves: &dyn Fn(&str) -> Vec<Box<dyn Sleeve>>) -> SimResult {
    let mut calendar: Vec<NaiveDate> = data.iter().flat_map(|s| s.days.iter().map(|d| d.date)).collect();
    calendar.sort();
    calendar.dedup();

    let mut res = SimResult::default();
    let mut cash = cfg.initial_equity;
    let mut states: Vec<SymState> = data
        .iter()
        .map(|_| SymState { book: SymbolBook::default(), net_shares: 0.0, ledgers: BTreeMap::new() })
        .collect();
    let mut sleeve_sets: Vec<Vec<Box<dyn Sleeve>>> = data.iter().map(|s| make_sleeves(&s.symbol)).collect();
    let mut risk = cfg.risk.map(|p| {
        let r = RiskOverlay::new(p, cfg.initial_equity);
        match cfg.fixed_sizing_equity {
            Some(b) => r.with_fixed_base(b),
            None => r,
        }
    });
    let vw = VolWindows { cc: cfg.vol_cc_window, on: cfg.vol_on_window, atr: 14, sma: cfg.sma_window };
    let mut cursors = vec![0usize; data.len()];
    let mut last_close: Vec<f64> = data.iter().map(|_| 0.0).collect();

    for date in calendar {
        // equity at day start marks open positions at yesterday's close
        let equity_start = cash + states.iter().zip(&last_close).map(|(s, c)| s.net_shares * c).sum::<f64>();
        if let Some(r) = risk.as_mut() {
            r.start_day(date, equity_start);
        }
        let mut day_sleeve_pnl: BTreeMap<SleeveId, f64> = BTreeMap::new();

        // ── build today's working set (envelopes first: the contexts borrow them) ──
        let mut plan: Vec<(usize, usize)> = Vec::new(); // (symbol index, day index)
        for (si, sym) in data.iter().enumerate() {
            let i = cursors[si];
            let Some(today) = sym.days.get(i).filter(|d| d.date == date) else { continue };
            cursors[si] += 1;
            if i < cfg.warmup_days {
                last_close[si] = today.close();
                continue;
            }
            plan.push((si, i));
        }
        if plan.is_empty() {
            continue;
        }
        let envelopes: Vec<Option<NoiseEnvelope>> =
            plan.iter().map(|&(si, i)| NoiseEnvelope::fit(&data[si].days[..i], cfg.envelope_lookback, cfg.envelope_mult)).collect();
        let mut runs: Vec<DayRun> = Vec::with_capacity(plan.len());
        for (k, &(si, i)) in plan.iter().enumerate() {
            let sym = &data[si];
            let today = &sym.days[i];
            let is_last_day = i + 1 == sym.days.len();
            let n_sess = match cfg.live_session_bars {
                Some(n) if is_last_day => n.max(today.len()),
                _ => today.len(),
            };
            let hist = &sym.days[..i];
            let ctx = DayCtx {
                date,
                symbol: &sym.symbol,
                today,
                prev_close: hist.last().map(|d| d.close()).unwrap_or(today.open()),
                envelope: envelopes[k].as_ref(),
                vol: vol::compute(hist, vw),
                equity: cfg.fixed_sizing_equity.unwrap_or(equity_start),
                session_bars: n_sess,
                force_close: cfg.live_force_close && is_last_day,
            };
            for s in sleeve_sets[si].iter_mut() {
                s.on_day_start(&ctx);
            }
            for l in states[si].ledgers.values_mut() {
                l.pnl_today = 0.0;
                l.mark = ctx.prev_close;
            }
            let mark_px = ctx.prev_close;
            runs.push(DayRun { si, today, ctx, vwap: Vwap::new(), pending: Vec::new(), mark_px });
        }

        // ── minute loop, all symbols at bar `idx` ──
        let max_len = runs.iter().map(|r| r.today.len()).max().unwrap_or(0);
        for idx in 0..max_len {
            for r in runs.iter_mut() {
                if idx >= r.today.len() {
                    continue;
                }
                let bar = r.today.bars[idx];
                let vw_now = r.vwap.push(&bar);
                r.mark_px = bar.c;
                let sleeves = &mut sleeve_sets[r.si];
                let st = &mut states[r.si];
                let ctx = &r.ctx;
                let mut targets: Vec<Target> = sleeves.iter_mut().filter_map(|s| s.on_bar(ctx, idx, vw_now)).collect();
                // Risk overlay: Half/Halted scale *growing* exposure (a B
                // entry, an A entry or flip) — a shrink or a flatten always
                // passes, so a halted day still ends flat. The per-symbol cap
                // binds on the sleeve's net (all ensemble members), not on
                // the member alone.
                for t in targets.iter_mut() {
                    if let Some(rk) = risk.as_mut() {
                        let prev = st.book.exposure_of(t.sleeve, t.variant);
                        let grew = t.exposure.abs() > prev.abs() || t.exposure * prev < 0.0;
                        if grew {
                            t.exposure *= rk.exposure_scale();
                            let others = st.book.sleeve_exposure_excluding(t.sleeve, t.variant);
                            t.exposure = rk.clamp_member_exposure(t.exposure, others, ctx.is_close(idx));
                        }
                    }
                    st.book.set(*t);
                }
                // Intraday decisions delayed by `fill_delay_bars` reach the
                // market at a later bar's close (execution-latency stress);
                // auction fills (open / closing bar) are never delayed.
                let due_now: Vec<SleeveId> = r.pending.iter().filter(|(at, _)| *at <= idx).flat_map(|(_, ids)| ids.iter().copied()).collect();
                r.pending.retain(|(at, _)| *at > idx);
                if !due_now.is_empty() {
                    fill(&mut cash, st, &mut res, cfg, ctx, ctx.symbol, idx, bar.c, &due_now);
                }
                for fill_at in [FillAt::Open, FillAt::Close] {
                    let touched: Vec<SleeveId> = targets.iter().filter(|t| t.fill_at == fill_at).map(|t| t.sleeve).collect();
                    if touched.is_empty() {
                        continue;
                    }
                    let px = match fill_at {
                        FillAt::Open => bar.o,
                        FillAt::Close => bar.c,
                    };
                    let delayable = cfg.fill_delay_bars > 0 && fill_at == FillAt::Close && !ctx.is_close(idx) && idx > 0;
                    if delayable {
                        let at = (idx + cfg.fill_delay_bars).min(ctx.session_bars - 1);
                        r.pending.push((at, touched));
                        continue;
                    }
                    fill(&mut cash, st, &mut res, cfg, ctx, ctx.symbol, idx, px, &touched);
                }
            }
            // Portfolio mark after every symbol has traded this minute. A
            // fresh Halted verdict flattens the intraday sleeve everywhere at
            // this bar's close (plan §3.8.3: "A flat, nothing new today");
            // carried B shares stay (they are sold at the next open anyway).
            if let Some(rk) = risk.as_mut() {
                let was_halted = rk.state == RiskState::Halted;
                let mark: f64 = cash + runs.iter().map(|r| states[r.si].net_shares * r.mark_px).sum::<f64>();
                if rk.on_mark(mark) == RiskState::Halted && !was_halted && cfg.halt_flatten_intraday {
                    for r in runs.iter_mut() {
                        if idx >= r.today.len() {
                            continue;
                        }
                        let st = &mut states[r.si];
                        let mut any = false;
                        for t in st.book.members_mut(SleeveId::A) {
                            t.exposure = 0.0;
                            any = true;
                        }
                        if any {
                            fill(&mut cash, st, &mut res, cfg, &r.ctx, r.ctx.symbol, idx, r.today.bars[idx].c, &[SleeveId::A]);
                        }
                    }
                }
            }
        }

        for r in &runs {
            let st = &mut states[r.si];
            let close = r.today.close();
            for (id, l) in st.ledgers.iter_mut() {
                l.pnl_today += l.shares * (close - l.mark);
                l.mark = close;
                *day_sleeve_pnl.entry(*id).or_default() += l.pnl_today;
            }
            last_close[r.si] = close;
        }

        let equity_end = cash + states.iter().zip(&last_close).map(|(s, c)| s.net_shares * c).sum::<f64>();
        let n_prev = res.dates.len();
        res.dates.push(date);
        res.equity.push(equity_end);
        res.daily_returns.push(equity_end / equity_start - 1.0);
        // one aligned entry per sleeve per day (zero-filled for late starters)
        for (id, pnl) in day_sleeve_pnl {
            res.sleeve_pnl.entry(id).or_insert_with(|| vec![0.0; n_prev]).push(pnl);
        }
        for v in res.sleeve_pnl.values_mut() {
            if v.len() < res.dates.len() {
                v.push(0.0);
            }
        }
    }
    res.risk_halts = risk.map(|r| r.halts).unwrap_or(0);
    for (sym, st) in data.iter().zip(&states) {
        let m: BTreeMap<SleeveId, f64> = st.ledgers.iter().filter(|(_, l)| l.shares != 0.0).map(|(id, l)| (*id, l.shares)).collect();
        if !m.is_empty() {
            res.sleeve_shares.insert(sym.symbol.clone(), m);
        }
    }
    res
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
