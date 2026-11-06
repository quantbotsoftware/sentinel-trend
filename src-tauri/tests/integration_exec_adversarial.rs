//! P1 — adversarial property tests for the MERIDIAN execution planner
//! (`PLAN_2026-09-26_riscul_de_implementare.md`, item P1).
//!
//! `src/meridian/exec.rs` and `src/meridian/broker.rs` are compiled here via
//! `#[path]`: nothing inside the strategy-hash roots is modified, so
//! `ST_MERIDIAN_HASH` (`541c0a15a313` since R6) is untouched.
//!
//! **Run in the dev profile only:** `cargo test --test integration_exec_adversarial`.
//! `cargo test --release` on this package rebuilds `target/release/sentinel-trend`
//! — the binary the MERIDIAN daemon runs from (chat 9, 2026-09-26 22:05: it
//! happened; the old bytes were restored from `/proc/<pid>/exe`).
//!
//! History: the first version (Antigravity, 2026-09-26 21:58) counted
//! 10 537 "zero-cross violations" in 100 000 states with a checker that
//! compared every `Submit` against the *pre-plan* broker position. Chat 9
//! re-classified them: 8 489 were the open leg of a legitimate flip (cover →
//! `AwaitPosition(0)` → open; the position is 0 when that leg is sent) and
//! 2 048 were one latent defect in `Phase::OpgWindow` (see the dedicated
//! test below). That version also never asserted on the count. This version
//! uses a sequential model and asserts.

#[path = "../src/meridian/broker.rs"]
pub mod broker;

/// `exec.rs` pulls in `exec_tests.rs`, which imports `crate::meridian::broker`
/// and (R6) `crate::meridian::exec_apply`.
pub mod meridian {
    pub use super::{broker, exec, exec_apply};
}

#[path = "../src/meridian/exec.rs"]
pub mod exec;

#[path = "../src/meridian/exec_apply.rs"]
pub mod exec_apply;

use broker::{BrokerOrder, Tif};
use exec::{plan, residual, Action, Phase, PlanInput};

struct FastPrng {
    state: u64,
}

impl FastPrng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.state
    }

    fn next_f64_range(&mut self, min: f64, max: f64) -> f64 {
        let val = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        min + val * (max - min)
    }

    fn next_i64_range(&mut self, min: i64, max: i64) -> i64 {
        let span = (max - min + 1) as u64;
        min + (self.next_u64() % span) as i64
    }

    fn next_bool(&mut self) -> bool {
        (self.next_u64() & 1) == 1
    }

    fn next_phase(&mut self) -> Phase {
        match self.next_u64() % 6 {
            0 => Phase::Session,
            1 => Phase::ClosePreview,
            2 => Phase::LateClose,
            3 => Phase::AfterClose,
            4 => Phase::OpgWindow,
            _ => Phase::Idle,
        }
    }
}

fn is_exit(o: &BrokerOrder) -> bool {
    o.client_order_id.starts_with("mrd-opg-")
}

/// Sign change of the *current* position by one order.
fn crosses(pos: f64, qty: f64) -> bool {
    let r = pos + qty;
    pos != 0.0 && ((pos > 0.0 && r < 0.0) || (pos < 0.0 && r > 0.0))
}

#[test]
fn exec_plan_invariants_hold_on_100k_random_states() {
    let mut prng = FastPrng::new(0xDEAD_BEEF_1337_4242);
    let iterations = 100_000;
    let symbols = ["SPY", "QQQ"];
    let dates = ["2026-09-28", "2026-10-05", "2026-11-23"];

    // Diagnostics for the summary line (not assertions).
    let mut flips = 0u64;

    for i in 0..iterations {
        let symbol = symbols[(prng.next_u64() as usize) % symbols.len()];
        let date = dates[(prng.next_u64() as usize) % dates.len()];
        let bar_idx = prng.next_i64_range(0, 390) as usize;
        let bars_today = prng.next_i64_range(0, 390) as usize;
        let phase = prng.next_phase();
        let desired_net = prng.next_f64_range(-500.0, 500.0).round();
        let a_shares = prng.next_f64_range(-300.0, 300.0).round();
        let b_shares = desired_net - a_shares;
        let broker_qty = prng.next_f64_range(-500.0, 500.0).round();
        let auction = prng.next_bool();
        let secs_to_close = prng.next_i64_range(-600, 23400);

        let num_open = prng.next_i64_range(0, 4) as usize;
        let mut open_orders = Vec::new();
        for o_idx in 0..num_open {
            let tif = match prng.next_u64() % 3 {
                0 => Tif::Day,
                1 => Tif::Cls,
                _ => Tif::Opg,
            };
            let qty = prng.next_f64_range(-300.0, 300.0).round();
            if qty != 0.0 {
                let prefix = if prng.next_bool() { "mrd-opg-" } else { "mrd-" };
                open_orders.push(BrokerOrder {
                    id: format!("ord-{i}-{o_idx}"),
                    client_order_id: format!("{prefix}{date}-{symbol}-{o_idx}"),
                    symbol: symbol.to_string(),
                    qty,
                    tif,
                    status: "new".to_string(),
                });
            }
        }

        let input = PlanInput {
            symbol,
            date,
            bar_idx,
            bars_today,
            phase,
            desired_net,
            a_shares,
            b_shares,
            broker_qty,
            open: &open_orders,
            auction,
            secs_to_close,
        };
        let actions = plan(&input);

        // INVARIANT 1 — nothing after the close.
        if matches!(phase, Phase::AfterClose | Phase::Idle) {
            assert!(
                !actions.iter().any(|a| matches!(a, Action::Submit(_))),
                "INV1 [iter {i}]: Submit in {phase:?}: {actions:?}"
            );
        }

        // INVARIANT 2 — no single order crosses zero, evaluated SEQUENTIALLY:
        // `AwaitPosition{qty}` means the cover leg has filled and the broker
        // position is `qty` before the next submit. Since R6 (E6) `OpgWindow`
        // is checked too: strays are cancelled and the exit is exactly −position.
        {
            let mut pos = broker_qty;
            let mut awaited = false;
            for a in &actions {
                match a {
                    Action::AwaitPosition { qty, .. } => {
                        pos = *qty;
                        awaited = true;
                    }
                    Action::Submit(spec) => {
                        assert!(
                            !crosses(pos, spec.qty),
                            "INV2 [iter {i}]: {phase:?} pos {pos} qty {} crosses zero; broker {broker_qty} open {:?} actions {actions:?}",
                            spec.qty,
                            open_orders.iter().map(|o| (o.tif, o.qty, o.client_order_id.clone())).collect::<Vec<_>>()
                        );
                    }
                    Action::Cancel { .. } => {}
                }
            }
            if awaited {
                flips += 1;
                // A flip is exactly cover → await(0) → open, in this order.
                let idx_await = actions.iter().position(|a| matches!(a, Action::AwaitPosition { .. })).unwrap();
                let cover = actions[..idx_await].iter().rev().find_map(|a| match a {
                    Action::Submit(s) => Some(s.qty),
                    _ => None,
                });
                assert_eq!(cover, Some(-broker_qty), "INV2 [iter {i}]: cover leg must flatten exactly: {actions:?}");
                assert!(matches!(actions[idx_await], Action::AwaitPosition { qty, .. } if qty == 0.0), "INV2 [iter {i}]: await must be 0");
                assert!(
                    actions[idx_await + 1..].iter().filter(|a| matches!(a, Action::Submit(_))).count() == 1,
                    "INV2 [iter {i}]: exactly one open leg after await: {actions:?}"
                );
            }
        }

        // INVARIANT 3 — cancels precede submits.
        let mut seen_submit = false;
        for a in &actions {
            match a {
                Action::Submit(_) => seen_submit = true,
                Action::Cancel { .. } => assert!(!seen_submit, "INV3 [iter {i}]: Cancel after Submit: {actions:?}"),
                Action::AwaitPosition { .. } => {}
            }
        }

        // INVARIANT 4 — ≤ 1 net MOC per symbol in ClosePreview (live); no
        // new auction orders in LateClose (live); no auction orders on paper.
        let auction_submits = actions.iter().filter(|a| matches!(a, Action::Submit(s) if s.tif == Tif::Cls || s.tif == Tif::Opg)).count();
        if !auction {
            assert_eq!(auction_submits, 0, "INV4 [iter {i}]: auction order on paper: {actions:?}");
        }
        if phase == Phase::ClosePreview && auction {
            let cls = actions.iter().filter(|a| matches!(a, Action::Submit(s) if s.tif == Tif::Cls)).count();
            assert!(cls <= 1, "INV4 [iter {i}]: {cls} MOC submits in ClosePreview");
        }
        if phase == Phase::LateClose && auction {
            assert_eq!(auction_submits, 0, "INV4 [iter {i}]: auction order in LateClose: {actions:?}");
        }
        if matches!(phase, Phase::Session) {
            assert_eq!(auction_submits, 0, "INV4 [iter {i}]: auction order in Session: {actions:?}");
        }

        // INVARIANT 5 — OPG window TIF: Opg live, Day paper; one exit at most.
        if phase == Phase::OpgWindow {
            let submits: Vec<_> = actions.iter().filter_map(|a| if let Action::Submit(s) = a { Some(s) } else { None }).collect();
            assert!(submits.len() <= 1, "INV5 [iter {i}]: more than one OPG exit: {actions:?}");
            for s in submits {
                assert_eq!(s.tif, if auction { Tif::Opg } else { Tif::Day }, "INV5 [iter {i}]: wrong TIF in OpgWindow");
                assert!(s.client_order_id.starts_with("mrd-opg-"), "INV5 [iter {i}]: exit id prefix");
            }
        }

        // INVARIANT 6 — whole, non-zero quantities; unique, non-empty ids.
        let mut ids = std::collections::HashSet::new();
        for a in &actions {
            match a {
                Action::Submit(s) => {
                    assert_ne!(s.qty, 0.0, "INV6 [iter {i}]: zero qty");
                    assert_eq!(s.qty.round(), s.qty, "INV6 [iter {i}]: fractional qty {}", s.qty);
                    assert!(!s.client_order_id.is_empty(), "INV6 [iter {i}]: empty id");
                    assert!(ids.insert(s.client_order_id.clone()), "INV6 [iter {i}]: duplicate id {}", s.client_order_id);
                }
                Action::AwaitPosition { qty, .. } => assert_eq!(qty.round(), *qty, "INV6 [iter {i}]: fractional await"),
                Action::Cancel { client_order_id, .. } => assert!(!client_order_id.is_empty(), "INV6 [iter {i}]: empty cancel id"),
            }
        }

        // INVARIANT 7 — idempotence: on the target state with no open
        // orders, the Session planner sends nothing.
        if phase == Phase::Session && open_orders.is_empty() {
            let again = plan(&PlanInput { broker_qty: desired_net, open: &[], ..input.clone() });
            assert!(again.is_empty(), "INV7 [iter {i}]: re-plan on target produced {again:?}");
        }

        // INVARIANT 8 — residual = close book − (position + in-flight day).
        if phase == Phase::AfterClose {
            let inflight: f64 = open_orders.iter().filter(|o| o.tif == Tif::Day).map(|o| o.qty).sum();
            assert_eq!(residual(&input), (desired_net - (broker_qty + inflight)).round(), "INV8 [iter {i}]");
        }
    }

    println!("P1: {iterations} states; {flips} flips (cover→await→open) verified sequentially; OpgWindow included since R6 E6");
}

/// Latent defect found in chat 9d, FIXED at R6 (E6, 2026-09-27): in
/// `Phase::OpgWindow` the exit used to be sized as `−(position + in-flight
/// non-exit Day orders)`; a same-sign in-flight order made the exit overshoot
/// and cross zero. R6 cancels every non-exit Day order in the window and
/// sizes the exit as exactly `−position`. This test was inverted when the fix
/// landed (it pinned the old behaviour, `−351`, until then).
#[test]
fn opg_window_same_sign_inflight_day_order_is_cancelled_and_exit_is_minus_position_r6_e6() {
    let open = [BrokerOrder {
        id: "ord-late".into(),
        client_order_id: "mrd-late-2026-09-28-SPY-388".into(),
        symbol: "SPY".into(),
        qty: 212.0,
        tif: Tif::Day,
        status: "new".into(),
    }];
    let i = PlanInput {
        symbol: "SPY",
        date: "2026-09-29",
        bar_idx: 0,
        bars_today: 0,
        phase: Phase::OpgWindow,
        desired_net: 0.0,
        a_shares: 0.0,
        b_shares: 0.0,
        broker_qty: 139.0,
        open: &open,
        auction: true,
        secs_to_close: 23400,
    };
    let actions = plan(&i);
    assert!(matches!(actions[0], Action::Cancel { ref id, .. } if id == "ord-late"), "R6 E6: the stray Day order is cancelled first: {actions:?}");
    let exit = actions.iter().find_map(|a| if let Action::Submit(s) = a { Some(s) } else { None }).expect("one exit");
    assert_eq!(exit.qty, -139.0, "R6 E6: the exit is exactly −position, never netted against a stray Day order");
    assert!(!crosses(i.broker_qty, exit.qty));
    assert!(!open.iter().any(is_exit));

    // Opposite-sign stray (a late cover that never filled): also cancelled, exit still −position.
    let open_ok = [BrokerOrder { qty: -100.0, ..open[0].clone() }];
    let ok = plan(&PlanInput { open: &open_ok, ..i.clone() });
    assert!(matches!(ok[0], Action::Cancel { .. }));
    let exit_ok = ok.iter().find_map(|a| if let Action::Submit(s) = a { Some(s) } else { None }).expect("one exit");
    assert_eq!(exit_ok.qty, -139.0);
    assert!(!crosses(i.broker_qty, exit_ok.qty));
}
