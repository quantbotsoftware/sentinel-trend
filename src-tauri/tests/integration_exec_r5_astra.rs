//! Runda 5 / Q4 (chat 10, 2026-09-27) — property tests for the R6 executor
//! derived from the red-team round with GPT-6 Astra and from Claude's sealed
//! position (`docs/meridian/validation/2026-09-27_runda5_pozitia_claude.md`).
//!
//! `exec.rs`, `exec_apply.rs`, `broker.rs` are compiled via `#[path]`: nothing
//! inside the strategy-hash roots changes (`541c0a15a313` stays frozen for the
//! 20 paper sessions). **Dev profile only**: `cargo test --test integration_exec_r5_astra`.
//!
//! Two defects are PINNED here (tests assert today's behaviour and say what the
//! fix is); both are queued for R7, neither is reachable on paper (`auction = false`):
//!
//!   * **D-R5-1 — `LateClose` (live) trusts the *existence* of a MOC, not its
//!     quantity.** `covered = open.any(Cls)`. A MOC placed correctly at 15:49 is
//!     never cancelled or corrected when the target changes after 15:50 (the live
//!     rail `live_risk::apply` cutting B after an intraday loss, or a revised
//!     `b_shares`). At the close the broker holds the old quantity; AfterClose
//!     logs the residual; the position is carried overnight against the rail.
//!     Fix: `covered ⇔ position + Σ Cls == desired_net`; otherwise cancel the MOC
//!     (→ Replan) and fall back to a day market order while the market is open.
//!   * **D-R5-2 — client_order_id can repeat between polls of the same bar.**
//!     Session ids are `mrd-{date}-{sym}-{bar}-{open.len()}`, live fallback ids
//!     `mrd-late-{date}-{sym}-{bar}`; two polls in one bar with the same open-list
//!     length (e.g. an order filled, then the rail changed the target) produce the
//!     same id ⇒ Alpaca 422 ⇒ the second order is never created (progress, not
//!     safety; the next bar has a new id). Fix: add `-t{secs_to_close}` as E4 did
//!     for MOC / OPG / close-mkt.
//!
//! Astra's round-5 safety invariants (Q4 automaton) and where each is checked:
//!   * "a MOC is cover only w.r.t. quantity and target version" → D-R5-1 here (pinned);
//!   * "one id never names two intentions" → D-R5-2 here (pinned) + E4 control;
//!   * "after a cancel the old plan authorises no submit" → `apply_e1_e2_tests`,
//!     `claude_injection_tests` (in `exec_tests.rs`, compiled here);
//!   * "a confirmed zero position alone does not authorise the opening budget" —
//!     not testable in `apply` (the rail lives in the driver); R7 design item;
//!   * "no open orders ≠ no processable requests" (lost-response submit) — needs a
//!     persistent intent register the code does not have; R7 design item.

#[path = "../src/meridian/broker.rs"]
pub mod broker;

pub mod meridian {
    pub use super::{broker, exec, exec_apply};
}

#[path = "../src/meridian/exec.rs"]
pub mod exec;

#[path = "../src/meridian/exec_apply.rs"]
pub mod exec_apply;

use broker::{BrokerOrder, Tif};
use exec::{plan, Action, Phase, PlanInput};

fn cls(id: &str, qty: f64) -> BrokerOrder {
    BrokerOrder { id: id.into(), client_order_id: format!("mrd-cls-2026-09-28-SPY-net-t{id}"), symbol: "SPY".into(), qty, tif: Tif::Cls, status: "accepted".into() }
}

fn late<'a>(desired: f64, b: f64, broker: f64, open: &'a [BrokerOrder], secs: i64) -> PlanInput<'a> {
    PlanInput { symbol: "SPY", date: "2026-09-28", bar_idx: 385, bars_today: 386, phase: Phase::LateClose, desired_net: desired, a_shares: 0.0, b_shares: b, broker_qty: broker, open, auction: true, secs_to_close: secs }
}

/// Sequential model of one poll: apply the plan's actions to (position, open).
/// Cancels remove the order; Submits add it (Cls stays open until the close;
/// Day is assumed filled — the most favourable case for the planner).
fn book_after(i: &PlanInput, actions: &[Action]) -> (f64, Vec<BrokerOrder>) {
    let mut pos = i.broker_qty;
    let mut open: Vec<BrokerOrder> = i.open.to_vec();
    for a in actions {
        match a {
            Action::Cancel { id, .. } => open.retain(|o| &o.id != id),
            Action::AwaitPosition { qty, .. } => pos = *qty,
            Action::Submit(s) => {
                if s.tif == Tif::Day {
                    pos += s.qty;
                } else {
                    open.push(BrokerOrder { id: s.client_order_id.clone(), client_order_id: s.client_order_id.clone(), symbol: s.symbol.clone(), qty: s.qty, tif: s.tif, status: "accepted".into() });
                }
            }
        }
    }
    (pos, open)
}

/// Close invariant (Astra r2, "Close": position + executions still possible ⇒
/// the B target — not "a Cls exists"). Evaluated at the close: Cls fill.
fn book_at_close(pos: f64, open: &[BrokerOrder]) -> f64 {
    pos + open.iter().filter(|o| o.tif == Tif::Cls).map(|o| o.qty).sum::<f64>()
}

// ---------------------------------------------------------------- D-R5-1

#[test]
fn d_r5_1_late_close_live_ignores_a_moc_whose_qty_no_longer_matches_the_target() {
    // 15:49:05 — MOC +100 accepted for B. 15:55 — the live rail halts (−1.5 %
    // realised intraday) and hands desired_net = 0 / b = 0. Market still open.
    let open = [cls("605", 100.0)];
    let i = late(0.0, 0.0, 0.0, &open, 300);
    let actions = plan(&i);
    assert!(actions.is_empty(), "PINNED (R7): today LateClose does nothing because a Cls exists: {actions:?}");
    let (pos, open_after) = book_after(&i, &actions);
    assert_eq!(book_at_close(pos, &open_after), 100.0, "…so the broker carries +100 overnight against a target of 0");

    // Same with a revised (smaller) B target: MOC +100 stays, target +40.
    let i2 = late(40.0, 40.0, 0.0, &open, 300);
    assert!(plan(&i2).is_empty(), "PINNED (R7): +60 over the B target is not corrected");

    // Control: no Cls ⇒ the fallback day market order reconciles to the target.
    let i3 = late(40.0, 40.0, 0.0, &[], 300);
    let a3 = plan(&i3);
    assert!(matches!(a3.as_slice(), [Action::Submit(s)] if s.qty == 40.0 && s.tif == Tif::Day), "{a3:?}");
    // Control: paper (`auction = false`) reconciles the whole close book in the last 45 s.
    let i4 = PlanInput { auction: false, secs_to_close: 30, ..late(0.0, 0.0, 0.0, &open, 30) };
    let a4 = plan(&i4);
    assert!(a4.iter().any(|a| matches!(a, Action::Cancel { .. })), "paper cancels the Cls: {a4:?}");
}

/// Random LateClose (live) states: every violation of the Close invariant must
/// be of class D-R5-1 (a Cls exists and position + Σ Cls ≠ desired). Any other
/// class would be a NEW defect and fails this test.
#[test]
fn d_r5_1_is_the_only_close_invariant_violation_class_in_late_close_live() {
    let mut seed = 0x5EED_2026_0927u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        seed
    };
    let mut violations = 0u64;
    let mut checked = 0u64;
    for n in 0..100_000u64 {
        let desired = ((next() % 801) as f64 - 400.0).round();
        let broker = ((next() % 801) as f64 - 400.0).round();
        let secs = (next() % 600) as i64 + 1;
        let mut open = Vec::new();
        if next() % 2 == 0 {
            open.push(cls(&format!("c{n}"), ((next() % 801) as f64 - 400.0).round()));
        }
        if next() % 3 == 0 {
            open.push(BrokerOrder { id: format!("d{n}"), client_order_id: format!("mrd-2026-09-28-SPY-380-{n}"), symbol: "SPY".into(), qty: ((next() % 401) as f64 - 200.0).round(), tif: Tif::Day, status: "new".into() });
        }
        open.retain(|o| o.qty != 0.0);
        let i = late(desired, desired, broker, &open, secs);
        let actions = plan(&i);
        let (pos, after) = book_after(&i, &actions);
        // in-flight Day orders that remain are assumed to fill before the close
        let day: f64 = after.iter().filter(|o| o.tif == Tif::Day).map(|o| o.qty).sum();
        let at_close = book_at_close(pos + day, &after);
        checked += 1;
        if at_close != desired {
            violations += 1;
            let has_cls = i.open.iter().any(|o| o.tif == Tif::Cls);
            assert!(has_cls && actions.is_empty(), "NEW Close-invariant violation class [iter {n}]: desired {desired} broker {broker} open {:?} actions {actions:?} at_close {at_close}", i.open.iter().map(|o| (o.tif, o.qty)).collect::<Vec<_>>());
        }
    }
    println!("D-R5-1: {violations} / {checked} random LateClose(live) states miss the close target; all of them are the 'a Cls exists' class");
    assert!(violations > 0, "PINNED: the defect is reachable in the state space; when fixed, drop this assertion");
}

// ---------------------------------------------------------------- D-R5-2

#[test]
fn d_r5_2_session_and_late_ids_repeat_between_polls_of_the_same_bar() {
    let base = PlanInput { symbol: "SPY", date: "2026-09-28", bar_idx: 61, bars_today: 62, phase: Phase::Session, desired_net: 100.0, a_shares: 100.0, b_shares: 0.0, broker_qty: 0.0, open: &[], auction: true, secs_to_close: 19_678 };
    let first = plan(&base);
    // the +100 filled; the live rail then halves A within the same bar (or a
    // restart re-plans): position 100 → target 50, no open orders, same bar.
    let second = plan(&PlanInput { desired_net: 50.0, a_shares: 50.0, broker_qty: 100.0, ..base.clone() });
    let id = |acts: &[Action]| acts.iter().find_map(|a| if let Action::Submit(s) = a { Some(s.client_order_id.clone()) } else { None }).expect("one submit");
    assert_eq!(id(&first), id(&second), "PINNED (R7): two distinct intentions in one bar share `mrd-2026-09-28-SPY-61-0` ⇒ 422 on the second");

    // live fallback: same bar, two polls, target changed between them
    let l1 = plan(&late(40.0, 40.0, 0.0, &[], 500));
    let l2 = plan(&late(20.0, 20.0, 40.0, &[], 480));
    assert_eq!(id(&l1), id(&l2), "PINNED (R7): `mrd-late-2026-09-28-SPY-385` repeats");

    // Control — E4 delivered: MOC / OPG / close-mkt ids carry `-t{secs}` and differ per poll.
    let cp = |secs: i64| PlanInput { phase: Phase::ClosePreview, secs_to_close: secs, ..late(100.0, 100.0, 0.0, &[], secs) };
    assert_ne!(id(&plan(&cp(700))), id(&plan(&cp(690))));
}
