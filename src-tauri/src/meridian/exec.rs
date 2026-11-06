//! MERIDIAN execution planner (phase 4b) — desired state vs. broker truth.
//!
//! Invariants (shadow period `2026-09-16_shadow_twin.md`, paper weeks
//! 2026-09-16 and 2026-09-21→25 — ANALIZA chat 5 and chat 7):
//!   * Alpaca holds shares for an open sell order (`held_for_orders`) and
//!     rejects opposite-side open orders ("potential wash trade"), so an
//!     intraday MOC "protection" is NOT possible: on 2026-09-16 it blocked
//!     two A exits (403). Intraday: day market orders only, and every
//!     open `mrd-*` order is cancelled before a submit that changes side.
//!   * **One order never crosses zero.** Alpaca refuses an order that
//!     closes a position and opens the opposite one ("insufficient qty
//!     available for order", 2026-09-23 15:49: short −171 → MOC +301, then
//!     the same 403 every 10 s until 16:15; A slept short, B never entered).
//!     Every flip is two legs: cover to flat (day market), confirm the
//!     position, then open (`AwaitPosition` between them).
//!   * **Auction orders exist only on live.** Paper Alpaca fills `cls`/`opg`
//!     as market orders at the NBBO with random partial fills and expires
//!     the rest (3/3 MOC and 2/2 OPG failed 2026-09-21→24; Alpaca staff on
//!     the forum confirm). `auction = false` emulates them: the close book
//!     is bought/sold as a day market order in the last `CLOSE_MARKET_SECS`
//!     before the close, B's exit is a day market order queued before the
//!     open (Alpaca executes queued day orders at the open).
//!   * **Nothing is sent after the close.** A day market order after 16:00
//!     queues to the next open — the 2026-09-21/22 fallback turned an
//!     expired MOC into an overnight carry of A at 2.65× gross. AfterClose
//!     observes and logs the residual; the OPG window fixes it.
//!   * The close is protected by the 15:48 ET preview: ONE net MOC per
//!     symbol (A flat + B carry) before the 15:50 cutoff on live; LateClose
//!     (15:50 → close) falls back to a day market order while the market is
//!     still open.
//!   * A cancel is confirmed gone (`open_orders`) before the next submit:
//!     `pending_cancel` still holds the shares (2026-09-24 09:31, L18).
//!
//! `plan` is pure and unit-tested (`exec_tests.rs`); `exec_apply::apply` performs
//! the calls (R6: re-plan after any cancel, phase deadline on every submit).

use serde::Serialize;

use super::broker::{BrokerOrder, OrderSpec, Tif};


/// Market-emulation window before the close (paper): the close book is
/// sent as a day market order once this close is this near.
pub const CLOSE_MARKET_SECS: i64 = 45;

/// Time-of-day phase, decided by the driver from the ET clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 09:30 ≤ t < 15:48 — regular decisions.
    Session,
    /// 15:48 ≤ t < 15:50 — close preview: one net MOC per symbol (live).
    ClosePreview,
    /// 15:50 ≤ t < close — no new auction orders; day-market fallback (live)
    /// or the market emulation of the close (paper, last 45 s).
    LateClose,
    /// close ≤ t ≤ close + 15 — observe only; a residual is logged.
    AfterClose,
    /// 19:00–19:15 or 09:00–09:27 — B exit as OPG (live) or queued day market.
    OpgWindow,
    /// Anything else: observe only.
    Idle,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanInput<'a> {
    pub symbol: &'a str,
    pub date: &'a str,
    pub bar_idx: usize,
    /// Bars of today's session already in the cache (0 before 09:31).
    pub bars_today: usize,
    pub phase: Phase,
    /// Desired net shares after the latest replayed bar.
    pub desired_net: f64,
    /// Sleeve A's share of the desired net (flat at the close).
    pub a_shares: f64,
    /// Sleeve B's share of the desired net (carried overnight).
    pub b_shares: f64,
    /// Broker position (signed) and open `mrd-*` orders for the symbol.
    pub broker_qty: f64,
    pub open: &'a [BrokerOrder],
    /// The broker executes auction orders (MOC / OPG) — live. False on paper.
    pub auction: bool,
    /// Seconds until the scheduled close (negative after it).
    pub secs_to_close: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Action {
    Submit(OrderSpec),
    Cancel { id: String, client_order_id: String, symbol: String },
    /// Block until the broker position of `symbol` equals `qty` (the cover
    /// leg of a flip has filled) before the next submit.
    AwaitPosition { symbol: String, qty: f64 },
}

pub(super) fn whole(x: f64) -> f64 {
    x.round()
}

fn is_exit(o: &BrokerOrder) -> bool {
    o.client_order_id.starts_with("mrd-opg-")
}

fn inflight_day(open: &[BrokerOrder]) -> f64 {
    open.iter().filter(|o| o.tif == Tif::Day).map(|o| o.qty).sum()
}

fn cancel(o: &BrokerOrder) -> Action {
    Action::Cancel { id: o.id.clone(), client_order_id: o.client_order_id.clone(), symbol: o.symbol.clone() }
}

fn cancels(open: &[BrokerOrder], tif: Tif) -> Vec<Action> {
    open.iter().filter(|o| o.tif == tif).map(cancel).collect()
}

fn submit(sym: &str, qty: f64, tif: Tif, id: String) -> Action {
    Action::Submit(OrderSpec { symbol: sym.into(), qty, tif, client_order_id: id })
}

/// Bring the symbol to `target` with market orders (`tif` for the opening
/// leg — Day intraday, Cls for a live close). Nets against in-flight day
/// orders, cancels opposite ones first, and splits a flip into cover →
/// confirm → open because one order may not cross zero.
fn reconcile_market(out: &mut Vec<Action>, i: &PlanInput, target: f64, tif: Tif, id: &str) {
    let sym = i.symbol;
    let target = whole(target);
    let effective = i.broker_qty + inflight_day(i.open);
    if whole(target - effective) == 0.0 {
        return;
    }
    let crosses = i.broker_qty != 0.0 && target != 0.0 && i.broker_qty.signum() != target.signum();
    if crosses {
        out.extend(cancels(i.open, Tif::Day));
        out.push(submit(sym, -i.broker_qty, Tif::Day, format!("{id}-c")));
        out.push(Action::AwaitPosition { symbol: sym.into(), qty: 0.0 });
        out.push(submit(sym, target, tif, id.into()));
        return;
    }
    let delta = whole(target - effective);
    // an in-flight day order on the opposite side must go before the new one
    let opposite: Vec<&BrokerOrder> = i.open.iter().filter(|o| o.tif == Tif::Day && o.qty.signum() != delta.signum()).collect();
    let cancelled_qty: f64 = opposite.iter().map(|o| o.qty).sum();
    out.extend(opposite.iter().map(|o| cancel(o)));
    let delta = whole(delta + cancelled_qty);
    if delta != 0.0 {
        out.push(submit(sym, delta, tif, id.into()));
    }
}

/// Pure: what to send/cancel for one symbol right now.
pub fn plan(i: &PlanInput) -> Vec<Action> {
    let mut out = Vec::new();
    let sym = i.symbol;
    match i.phase {
        Phase::Session => {
            // 09:30:02: B's exit (OPG on live, queued day market on paper) is
            // in the opening auction and its fill may not be reported yet —
            // cancelling it here would turn the auction exit into a 09:31
            // market sell. Wait for bar 0.
            if i.bars_today == 0 && i.open.iter().any(is_exit) {
                return out;
            }
            // leftovers from other phases (a stale MOC / live OPG / paper
            // `mrd-opg-*` day order) would hold shares or trigger the
            // wash-trade reject — clear them first
            out.extend(cancels(i.open, Tif::Cls));
            out.extend(cancels(i.open, Tif::Opg));
            out.extend(i.open.iter().filter(|o| is_exit(o) && o.tif == Tif::Day).map(cancel));
            // leftover exits are cancelled above — do not count them as in-flight
            let rest: Vec<BrokerOrder> = i.open.iter().filter(|o| o.tif == Tif::Day && !is_exit(o)).cloned().collect();
            let id = format!("mrd-{}-{sym}-{}-{}", i.date, i.bar_idx, i.open.len());
            reconcile_market(&mut out, &PlanInput { open: &rest, ..i.clone() }, i.desired_net, Tif::Day, &id);
        }
        Phase::ClosePreview => {
            if !i.auction {
                return out; // paper: the close is emulated in LateClose
            }
            // one net MOC: from the effective position to tonight's B target;
            // a flip (A short, B long) covers A now and sends B's MOC alone
            let effective = i.broker_qty + inflight_day(i.open);
            let want = whole(i.b_shares - effective);
            let have: Vec<&BrokerOrder> = i.open.iter().filter(|o| o.tif == Tif::Cls).collect();
            let matches = have.len() == 1 && have[0].qty == want;
            if !matches {
                // any open day order would hold shares / wash-trade the MOC
                out.extend(cancels(i.open, Tif::Day));
                out.extend(cancels(i.open, Tif::Cls));
                // R6 E5: reconcile to B's target unconditionally — `want == 0`
                // only meant "covered by in-flight orders", and those were
                // just cancelled. No-op when the position already equals
                // `b_shares`. (After a cancel `apply` re-plans anyway.)
                reconcile_market(&mut out, &PlanInput { open: &[], ..i.clone() }, i.b_shares, Tif::Cls, &format!("mrd-cls-{}-{sym}-net-t{}", i.date, i.secs_to_close));
            }
        }
        Phase::LateClose => {
            // `desired_net` is the *close target* (A flat, B as decided at
            // 15:48): the driver hands the close-preview snapshot until the
            // closing bar itself is replayed (ANALIZA chat 5 L24).
            if i.auction {
                // Auction orders can no longer be placed. If A is still exposed
                // and nothing covers it, fall back to a day market order while
                // the market is open (never after: it would queue to the open).
                let covered = i.open.iter().any(|o| o.tif == Tif::Cls);
                if !covered && i.secs_to_close > 0 {
                    reconcile_market(&mut out, i, i.desired_net, Tif::Day, &format!("mrd-late-{}-{sym}-{}", i.date, i.bar_idx));
                }
            } else if (0..=CLOSE_MARKET_SECS).contains(&i.secs_to_close) {
                // paper: the whole close book as one day market order ~30 s
                // before the close (fills at the NBBO; the engine fills at the
                // closing print — a measured, ~30 s divergence)
                out.extend(cancels(i.open, Tif::Cls));
                reconcile_market(&mut out, i, i.desired_net, Tif::Day, &format!("mrd-cls-{}-{sym}-mkt-t{}", i.date, i.secs_to_close));
            }
        }
        Phase::OpgWindow => {
            // B exit at the open: sell (or cover) the whole overnight position
            // — one `mrd-opg-*` order, OPG on live, day market queued to the
            // open on paper.
            // R6 E6 (chat 9d/9e): a stray non-exit Day order here (only
            // reachable after a submit that crossed the close, E2) is
            // cancelled, never netted — same-sign netting made the exit
            // cross zero. The exit is exactly −position.
            let strays: Vec<&BrokerOrder> = i.open.iter().filter(|o| o.tif == Tif::Day && !is_exit(o)).collect();
            let want = -whole(i.broker_qty);
            let exits: Vec<&BrokerOrder> = i.open.iter().filter(|o| is_exit(o)).collect();
            let has = strays.is_empty() && exits.len() == 1 && exits[0].qty == want;
            if !has {
                out.extend(strays.iter().map(|o| cancel(o)));
                out.extend(exits.iter().map(|o| cancel(o)));
                if want != 0.0 {
                    let tif = if i.auction { Tif::Opg } else { Tif::Day };
                    out.push(submit(sym, want, tif, format!("mrd-opg-{}-{sym}-t{}", i.date, i.secs_to_close)));
                }
            }
        }
        Phase::AfterClose | Phase::Idle => {}
    }
    out
}

/// Shares by which the broker book (position + in-flight day orders) misses
/// the close book — logged by the driver in AfterClose (nothing is sent).
pub fn residual(i: &PlanInput) -> f64 {
    whole(i.desired_net - (i.broker_qty + inflight_day(i.open)))
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
