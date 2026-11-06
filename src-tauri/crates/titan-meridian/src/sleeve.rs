//! Sleeve abstraction: a sleeve turns market context into *target
//! exposures*; it never touches orders. Netting turns targets into deltas.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::bars::DaySeries;
use crate::envelope::NoiseEnvelope;
use crate::vol::VolStats;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum SleeveId {
    /// Intraday momentum on index ETFs (noise envelope + VWAP trail).
    A,
    /// Overnight carry on index ETFs (IBS + trend filtered).
    B,
    /// Stocks-in-play opening range breakout.
    C,
    /// Variance risk premium (options) — signal only in this crate.
    D,
}

impl SleeveId {
    pub fn as_str(&self) -> &'static str {
        match self {
            SleeveId::A => "A",
            SleeveId::B => "B",
            SleeveId::C => "C",
            SleeveId::D => "D",
        }
    }
}

/// Which price of the current bar a target is assumed to trade at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillAt {
    Open,
    Close,
}

/// Desired exposure for one symbol after the current bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub sleeve: SleeveId,
    /// Member of an ensemble inside the sleeve (A runs one instance per band
    /// multiplier k). Attribution, halts and the live A/B split are per
    /// `sleeve`; only the netting book keys on `(sleeve, variant)`.
    pub variant: u8,
    /// Signed fraction of equity (+1.0 = 100 % long). `0.0` = flat.
    pub exposure: f64,
    /// Price used to convert exposure into a share count (e.g. session open).
    pub price_ref: f64,
    pub fill_at: FillAt,
}

/// Per-symbol, per-day context. Sleeves MUST only read `today.bars[..=idx]`
/// — the simulator hands the full day for convenience, live hands a
/// growing series. `envelope`/`vol` are as of the previous close.
pub struct DayCtx<'a> {
    pub date: NaiveDate,
    pub symbol: &'a str,
    pub today: &'a DaySeries,
    pub prev_close: f64,
    pub envelope: Option<&'a NoiseEnvelope>,
    pub vol: VolStats,
    /// Equity at the start of the day (sizing base).
    pub equity: f64,
    /// Scheduled bars in this session (390 / 210). Equals `today.len()` in
    /// historical simulation; in live replay of a *partial* day it is the
    /// scheduled length, so "close" logic fires only at the real close.
    pub session_bars: usize,
    /// Live "close preview": treat the last *available* bar as the closing
    /// bar (used at 15:48 ET to derive market-on-close orders). False in
    /// historical simulation.
    pub force_close: bool,
}

impl DayCtx<'_> {
    /// Is bar `idx` the closing bar for this session?
    pub fn is_close(&self, idx: usize) -> bool {
        idx + 1 == self.session_bars || (self.force_close && idx + 1 == self.today.len())
    }
}

pub trait Sleeve {
    fn id(&self) -> SleeveId;
    /// Called once at the start of each session (state reset, sizing).
    fn on_day_start(&mut self, ctx: &DayCtx);
    /// Called after every bar `idx` with the running session VWAP.
    /// Returns a new target only when the desired exposure changes.
    fn on_bar(&mut self, ctx: &DayCtx, idx: usize, vwap: f64) -> Option<Target>;
}
