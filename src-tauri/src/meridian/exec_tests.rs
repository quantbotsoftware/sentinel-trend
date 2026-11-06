//! Planner tests (kept apart from `exec.rs` for the 500-line rule).

use super::*;
use crate::meridian::broker::{BrokerOrder, Tif};
#[allow(unused_imports)]
use crate::meridian::exec_apply::{apply, BrokerApi, Outcome};

fn o(id: &str, qty: f64, tif: Tif) -> BrokerOrder {
    let cid = match id {
        "opg" => "mrd-opg-2026-09-16-SPY".into(),
        "cls" => "mrd-cls-2026-09-16-SPY-net".into(),
        "late" => "mrd-late-2026-09-16-SPY-389".into(),
        _ => format!("mrd-{id}"),
    };
    BrokerOrder { id: id.into(), client_order_id: cid, symbol: "SPY".into(), qty, tif, status: "new".into() }
}

fn input<'a>(phase: Phase, desired: f64, a: f64, b: f64, broker: f64, open: &'a [BrokerOrder]) -> PlanInput<'a> {
    PlanInput {
        symbol: "SPY",
        date: "2026-09-16",
        bar_idx: 29,
        bars_today: 30,
        phase,
        desired_net: desired,
        a_shares: a,
        b_shares: b,
        broker_qty: broker,
        open,
        auction: true,
        secs_to_close: 300,
    }
}

#[test]
fn session_waits_for_the_opening_auction_before_touching_an_opg() {
    let opg = [o("opg", -136.0, Tif::Opg)];
    let wait = PlanInput { bars_today: 0, bar_idx: 0, ..input(Phase::Session, 136.0, 0.0, 136.0, 136.0, &opg) };
    assert!(plan(&wait).is_empty());
    let late = PlanInput { bars_today: 1, bar_idx: 0, ..input(Phase::Session, 0.0, 0.0, 0.0, 136.0, &opg) };
    let acts = plan(&late);
    assert!(matches!(&acts[0], Action::Cancel { .. }));
    assert!(matches!(&acts[1], Action::Submit(s) if s.qty == -136.0 && s.tif == Tif::Day));
    let free = PlanInput { bars_today: 0, bar_idx: 0, ..input(Phase::Session, 0.0, 0.0, 0.0, 136.0, &[]) };
    assert!(matches!(&plan(&free)[0], Action::Submit(s) if s.qty == -136.0));
}

#[test]
fn session_cancels_an_unfilled_paper_opg_after_the_open() {
    // paper: the exit is a day market tagged mrd-opg-*; at 09:31 it is leftover
    let opg = [o("opg", -136.0, Tif::Day)];
    let late = PlanInput { bars_today: 1, bar_idx: 0, ..input(Phase::Session, 0.0, 0.0, 0.0, 136.0, &opg) };
    let acts = plan(&late);
    assert!(matches!(&acts[0], Action::Cancel { client_order_id, .. } if client_order_id.starts_with("mrd-opg-")));
    assert!(matches!(&acts[1], Action::Submit(s) if s.qty == -136.0 && s.tif == Tif::Day));
}

#[test]
fn session_entry_is_a_single_day_market_order() {
    let acts = plan(&input(Phase::Session, 560.0, 560.0, 0.0, 0.0, &[]));
    assert_eq!(acts.len(), 1, "no intraday MOC: Alpaca holds shares / rejects wash trades");
    let Action::Submit(m) = &acts[0] else { panic!() };
    assert_eq!((m.qty, m.tif), (560.0, Tif::Day));
}

#[test]
fn session_is_idempotent_when_converged_or_in_flight() {
    assert!(plan(&input(Phase::Session, 560.0, 560.0, 0.0, 560.0, &[])).is_empty());
    let open = [o("m", 560.0, Tif::Day)];
    assert!(plan(&input(Phase::Session, 560.0, 560.0, 0.0, 0.0, &open)).is_empty());
}

#[test]
fn session_exit_cancels_stale_auction_orders_before_selling() {
    let open = [o("cls", -560.0, Tif::Cls)];
    let acts = plan(&input(Phase::Session, 0.0, 0.0, 0.0, 560.0, &open));
    assert!(matches!(&acts[0], Action::Cancel { .. }));
    assert!(matches!(&acts[1], Action::Submit(s) if s.qty == -560.0 && s.tif == Tif::Day));
}

#[test]
fn session_flip_cancels_opposite_inflight_and_never_crosses_zero() {
    let open = [o("m", 560.0, Tif::Day)];
    let acts = plan(&input(Phase::Session, -498.0, -498.0, 0.0, 0.0, &open));
    assert!(matches!(&acts[0], Action::Cancel { .. }));
    assert!(matches!(&acts[1], Action::Submit(s) if s.qty == -498.0 && s.tif == Tif::Day), "{acts:?}");
    // filled long 560 → short 498: cover to flat, confirm, then open (2026-09-23)
    let acts2 = plan(&input(Phase::Session, -498.0, -498.0, 0.0, 560.0, &[]));
    assert!(matches!(&acts2[0], Action::Submit(s) if s.qty == -560.0 && s.tif == Tif::Day && s.client_order_id.ends_with("-c")));
    assert!(matches!(&acts2[1], Action::AwaitPosition { qty, .. } if *qty == 0.0));
    assert!(matches!(&acts2[2], Action::Submit(s) if s.qty == -498.0 && s.tif == Tif::Day));
}

#[test]
fn close_preview_nets_a_flat_and_b_entry_into_one_moc() {
    let acts = plan(&input(Phase::ClosePreview, 136.0, 0.0, 136.0, 560.0, &[]));
    assert_eq!(acts.len(), 1);
    assert!(matches!(&acts[0], Action::Submit(s) if s.qty == -424.0 && s.tif == Tif::Cls && s.client_order_id.contains("-net")));
    let have = [o("cls", -424.0, Tif::Cls)];
    assert!(plan(&input(Phase::ClosePreview, 136.0, 0.0, 136.0, 560.0, &have)).is_empty());
    let stale = [o("cls", -560.0, Tif::Cls)];
    let acts_r = plan(&input(Phase::ClosePreview, 136.0, 0.0, 136.0, 560.0, &stale));
    assert!(matches!(&acts_r[0], Action::Cancel { .. }) && matches!(&acts_r[1], Action::Submit(s) if s.qty == -424.0));
    assert!(plan(&input(Phase::ClosePreview, 0.0, 0.0, 0.0, 0.0, &[])).is_empty());
}

#[test]
fn close_preview_split_when_a_short_and_b_wants_long() {
    // 2026-09-23 15:49: A −171, B +130 ⇒ one MOC +301 was 403. Cover, then B's MOC.
    let acts = plan(&input(Phase::ClosePreview, 130.0, 0.0, 130.0, -171.0, &[]));
    assert!(matches!(&acts[0], Action::Submit(s) if s.qty == 171.0 && s.tif == Tif::Day && s.client_order_id.ends_with("-c")));
    assert!(matches!(&acts[1], Action::AwaitPosition { qty, .. } if *qty == 0.0));
    assert!(matches!(&acts[2], Action::Submit(s) if s.qty == 130.0 && s.tif == Tif::Cls && s.client_order_id.contains("-net")));
}

#[test]
fn close_preview_is_silent_on_paper() {
    let i = PlanInput { auction: false, ..input(Phase::ClosePreview, 136.0, 0.0, 136.0, 560.0, &[]) };
    assert!(plan(&i).is_empty(), "paper close is emulated in LateClose");
}

#[test]
fn late_close_only_falls_back_when_uncovered_and_the_market_is_open() {
    let covered = [o("cls", -560.0, Tif::Cls)];
    assert!(plan(&input(Phase::LateClose, 0.0, 0.0, 0.0, 560.0, &covered)).is_empty());
    let acts = plan(&input(Phase::LateClose, 0.0, 0.0, 0.0, 560.0, &[]));
    assert!(matches!(&acts[0], Action::Submit(s) if s.qty == -560.0 && s.tif == Tif::Day));
    let after = PlanInput { secs_to_close: -12, ..input(Phase::LateClose, 0.0, 0.0, 0.0, 560.0, &[]) };
    assert!(plan(&after).is_empty(), "a day market after 16:00 queues to the open (2026-09-21)");
}

#[test]
fn late_close_targets_the_close_book_not_the_intraday_one() {
    let acts = plan(&input(Phase::LateClose, 136.0, 0.0, 136.0, 560.0, &[]));
    assert!(matches!(&acts[0], Action::Submit(s) if s.qty == -424.0 && s.tif == Tif::Day && s.client_order_id.starts_with("mrd-late-")), "{acts:?}");
    assert!(plan(&input(Phase::LateClose, 136.0, 0.0, 136.0, 136.0, &[])).is_empty(), "MOC filled: converged");
    let late = [o("late", -424.0, Tif::Day)];
    assert!(plan(&input(Phase::LateClose, 136.0, 0.0, 136.0, 560.0, &late)).is_empty());
}

#[test]
fn late_close_paper_sends_the_close_book_only_in_the_last_seconds() {
    let early = PlanInput { auction: false, secs_to_close: 58, ..input(Phase::LateClose, 0.0, 0.0, 0.0, 195.0, &[]) };
    assert!(plan(&early).is_empty(), "15:59:02 is still outside the 45 s window");
    let now = PlanInput { auction: false, secs_to_close: 38, ..input(Phase::LateClose, 0.0, 0.0, 0.0, 195.0, &[]) };
    let acts = plan(&now);
    assert!(matches!(&acts[0], Action::Submit(s) if s.qty == -195.0 && s.tif == Tif::Day && s.client_order_id.contains("-mkt")), "{acts:?}");
    let flip = PlanInput { auction: false, secs_to_close: 38, ..input(Phase::LateClose, 130.0, 0.0, 130.0, -171.0, &[]) };
    let acts2 = plan(&flip);
    assert!(matches!(&acts2[0], Action::Submit(s) if s.qty == 171.0 && s.tif == Tif::Day && s.client_order_id.ends_with("-c")));
    assert!(matches!(&acts2[2], Action::Submit(s) if s.qty == 130.0 && s.tif == Tif::Day));
}

#[test]
fn after_close_never_sends_and_residual_is_the_miss() {
    let i = input(Phase::AfterClose, 0.0, 0.0, 0.0, 195.0, &[]);
    assert!(plan(&i).is_empty());
    assert_eq!(residual(&i), -195.0);
    assert_eq!(residual(&input(Phase::AfterClose, 130.0, 0.0, 130.0, 130.0, &[])), 0.0);
}

#[test]
fn opg_window_places_one_exit_for_the_overnight_position() {
    let acts = plan(&input(Phase::OpgWindow, 0.0, 0.0, 136.0, 136.0, &[]));
    assert!(matches!(&acts[0], Action::Submit(s) if s.qty == -136.0 && s.tif == Tif::Opg));
    let have = [o("opg", -136.0, Tif::Opg)];
    assert!(plan(&input(Phase::OpgWindow, 0.0, 0.0, 136.0, 136.0, &have)).is_empty());
    assert!(plan(&input(Phase::OpgWindow, 0.0, 0.0, 0.0, 0.0, &[])).is_empty());
    assert!(plan(&input(Phase::Idle, 0.0, 0.0, 0.0, 560.0, &[])).is_empty());
    // R6 E6: a stray Day order in the OPG window is cancelled, never netted;
    // the exit is exactly −position (pre-R6 netted −424 and exited −136).
    let late = [o("late", -424.0, Tif::Day)];
    let acts2 = plan(&input(Phase::OpgWindow, 0.0, 0.0, 136.0, 560.0, &late));
    assert!(matches!(&acts2[0], Action::Cancel { .. }), "{acts2:?}");
    assert!(matches!(&acts2[1], Action::Submit(s) if s.qty == -560.0 && s.tif == Tif::Opg), "{acts2:?}");
    // the same-sign case that motivated E6 (chat 9d): long +139, stray buy +212
    // ⇒ pre-R6 exit −351 would cross zero; now cancel + exit −139.
    let same_sign = [o("late", 212.0, Tif::Day)];
    let acts3 = plan(&input(Phase::OpgWindow, 0.0, 0.0, 139.0, 139.0, &same_sign));
    assert!(matches!(&acts3[0], Action::Cancel { .. }));
    assert!(matches!(&acts3[1], Action::Submit(s) if s.qty == -139.0 && s.tif == Tif::Opg), "{acts3:?}");
}

/// R6 E5: ClosePreview used to cancel a covering Day order when `want == 0`
/// and replace it with nothing; now the MOC to B's target is always planned
/// (apply stops after the cancel anyway and the next poll re-plans).
#[test]
fn close_preview_replaces_the_covering_day_order_it_cancels() {
    let day = [o("d", 50.0, Tif::Day)];
    // broker +100, Day +50 in flight, B wants +150 ⇒ effective 150, want 0
    let acts = plan(&input(Phase::ClosePreview, 150.0, 0.0, 150.0, 100.0, &day));
    assert!(matches!(&acts[0], Action::Cancel { .. }), "{acts:?}");
    assert!(matches!(&acts[1], Action::Submit(s) if s.qty == 50.0 && s.tif == Tif::Cls), "{acts:?}");
    // already at target with nothing open ⇒ silent
    assert!(plan(&input(Phase::ClosePreview, 150.0, 0.0, 150.0, 150.0, &[])).is_empty());
}

/// R6 E4: auction/close ids carry the poll's `secs_to_close`, so a replaced
/// MOC or OPG never reuses the id of the order it replaces (Alpaca 422).
#[test]
fn replacement_orders_never_reuse_a_client_order_id() {
    let first = input(Phase::ClosePreview, 136.0, 0.0, 136.0, 560.0, &[]);
    let id1 = match &plan(&first)[0] { Action::Submit(s) => s.client_order_id.clone(), a => panic!("{a:?}") };
    let stale = [BrokerOrder { id: "m1".into(), client_order_id: id1.clone(), symbol: "SPY".into(), qty: -560.0, tif: Tif::Cls, status: "new".into() }];
    let second = PlanInput { secs_to_close: 240, open: &stale, ..first.clone() };
    let acts = plan(&second);
    let id2 = acts.iter().find_map(|a| match a { Action::Submit(s) => Some(s.client_order_id.clone()), _ => None }).expect("replacement");
    assert_ne!(id1, id2, "replacement must carry a new client_order_id");
    assert!(id1.starts_with("mrd-cls-") && id2.starts_with("mrd-cls-"));
    // same for the OPG exit
    let e1 = match &plan(&input(Phase::OpgWindow, 0.0, 0.0, 136.0, 136.0, &[]))[0] { Action::Submit(s) => s.client_order_id.clone(), a => panic!("{a:?}") };
    let exit_open = [BrokerOrder { id: "x1".into(), client_order_id: e1.clone(), symbol: "SPY".into(), qty: -100.0, tif: Tif::Opg, status: "new".into() }];
    let later = PlanInput { secs_to_close: 250, open: &exit_open, ..input(Phase::OpgWindow, 0.0, 0.0, 136.0, 136.0, &[]) };
    let e2 = plan(&later).iter().find_map(|a| match a { Action::Submit(s) => Some(s.client_order_id.clone()), _ => None }).expect("replacement exit");
    assert_ne!(e1, e2);
    assert!(e2.starts_with("mrd-opg-"), "{e2}");
}

#[test]
fn opg_window_on_paper_is_a_queued_day_market() {
    let i = PlanInput { auction: false, ..input(Phase::OpgWindow, 0.0, 0.0, 136.0, 136.0, &[]) };
    assert!(matches!(&plan(&i)[0], Action::Submit(s) if s.qty == -136.0 && s.tif == Tif::Day && s.client_order_id.starts_with("mrd-opg-")));
}

mod apply_e1_e2_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Debug)]
    enum Event {
        Cancel {
            response_delay: Duration,
        },
        Orders {
            response_delay: Duration,
            fill: f64,
            disappear: bool,
        },
        Position {
            response_delay: Duration,
            qty: f64,
        },
    }

    #[derive(Debug, Clone, PartialEq)]
    struct RecordedSubmit {
        symbol: String,
        qty: f64,
        tif: String,
        client_order_id: String,
    }

    struct FakeState {
        position: f64,
        open: Vec<BrokerOrder>,
        events: VecDeque<Event>,
        submitted: Vec<RecordedSubmit>,
        cancelled: Vec<String>,
    }

    struct FakeBroker {
        state: Mutex<FakeState>,
    }

    impl FakeBroker {
        fn new(
            position: f64,
            open: Vec<BrokerOrder>,
            events: Vec<Event>,
        ) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    position,
                    open,
                    events: events.into_iter().collect(),
                    submitted: Vec::new(),
                    cancelled: Vec::new(),
                }),
            }
        }

        fn next_event(&self) -> Result<Event, String> {
            self.state
                .lock()
                .expect("fake state mutex poisoned")
                .events
                .pop_front()
                .ok_or_else(|| {
                    "fake broker: no scripted event left".to_owned()
                })
        }

        fn position_now(&self) -> f64 {
            self.state
                .lock()
                .expect("fake state mutex poisoned")
                .position
        }

        fn submissions(&self) -> Vec<RecordedSubmit> {
            self.state
                .lock()
                .expect("fake state mutex poisoned")
                .submitted
                .clone()
        }

        fn cancellations(&self) -> Vec<String> {
            self.state
                .lock()
                .expect("fake state mutex poisoned")
                .cancelled
                .clone()
        }
    }

    async fn pause_for(duration: Duration) {
        if !duration.is_zero() {
            tokio::time::sleep(duration).await;
        }
    }

    impl BrokerApi for FakeBroker {
        async fn position(
            &self,
            _symbol: &str,
        ) -> Result<f64, String> {
            match self.next_event()? {
                Event::Position {
                    response_delay,
                    qty,
                } => {
                    // The fill has happened at the broker, but the
                    // position response may still arrive late.
                    self.state
                        .lock()
                        .expect("fake state mutex poisoned")
                        .position = qty;

                    pause_for(response_delay).await;
                    Ok(qty)
                }
                other => Err(format!(
                    "fake position: unexpected event {other:?}"
                )),
            }
        }

        async fn open_orders(
            &self,
            _symbol: &str,
        ) -> Result<Vec<BrokerOrder>, String> {
            match self.next_event()? {
                Event::Orders {
                    response_delay,
                    fill,
                    disappear,
                } => {
                    pause_for(response_delay).await;

                    let mut state = self.state
                        .lock()
                        .expect("fake state mutex poisoned");

                    // Inject the fill after cancel acceptance and
                    // before the order disappears from open_orders.
                    state.position += fill;

                    if let Some(order) = state.open.first_mut() {
                        order.qty -= fill;
                    }

                    if disappear {
                        state.open.clear();
                    }

                    Ok(state.open.clone())
                }
                other => Err(format!(
                    "fake open_orders: unexpected event {other:?}"
                )),
            }
        }

        async fn submit(
            &self,
            spec: &OrderSpec,
        ) -> Result<String, String> {
            let mut state = self.state
                .lock()
                .expect("fake state mutex poisoned");

            let id = format!("fake-{}", state.submitted.len() + 1);

            state.submitted.push(RecordedSubmit {
                symbol: spec.symbol.clone(),
                qty: spec.qty,
                tif: spec.tif.as_str().to_owned(),
                client_order_id: spec.client_order_id.clone(),
            });

            Ok(id)
        }

        async fn cancel(
            &self,
            order_id: &str,
        ) -> Result<(), String> {
            self.state
                .lock()
                .expect("fake state mutex poisoned")
                .cancelled
                .push(order_id.to_owned());

            match self.next_event()? {
                Event::Cancel { response_delay } => {
                    // Acceptance alone does not remove the order.
                    pause_for(response_delay).await;
                    Ok(())
                }
                other => Err(format!(
                    "fake cancel: unexpected event {other:?}"
                )),
            }
        }
    }

    fn day_order(qty: f64) -> BrokerOrder {
        BrokerOrder {
            id: "day-1".to_owned(),
            client_order_id: "mrd-day-1".to_owned(),
            symbol: "SPY".to_owned(),
            qty,
            tif: Tif::Day,
            status: "new".to_owned(),
        }
    }

    fn cancel_then_stale_moc() -> Vec<Action> {
        vec![
            Action::Cancel {
                id: "day-1".to_owned(),
                client_order_id: "mrd-day-1".to_owned(),
                symbol: "SPY".to_owned(),
            },
            Action::Submit(OrderSpec {
                symbol: "SPY".to_owned(),
                qty: 100.0,
                tif: Tif::Cls,
                client_order_id:
                    "mrd-cls-2026-09-16-SPY-net".to_owned(),
            }),
        ]
    }

    fn cancel_events(fill: f64) -> Vec<Event> {
        vec![
            Event::Cancel {
                response_delay: Duration::ZERO,
            },
            Event::Orders {
                response_delay: Duration::ZERO,
                fill,
                disappear: true,
            },
        ]
    }

    fn deadline_in(duration: Duration) -> std::time::Instant {
        (tokio::time::Instant::now() + duration).into_std()
    }

    async fn assert_cancel_requires_replan(fill: f64) {
        let broker = FakeBroker::new(
            0.0,
            vec![day_order(60.0)],
            cancel_events(fill),
        );
        let actions = cancel_then_stale_moc();

        let (_, outcome) = apply(
            &broker,
            &actions,
            deadline_in(Duration::from_secs(30)),
        )
        .await
        .expect("cancel confirmation should succeed");

        assert_eq!(outcome, Outcome::Replan);
        assert_eq!(broker.position_now(), fill);
        assert_eq!(
            broker.cancellations(),
            vec!["day-1".to_owned()]
        );
        assert!(
            broker.submissions().is_empty(),
            "a pre-cancel MOC quantity must never be submitted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_full_fill_stops_before_stale_moc() {
        assert_cancel_requires_replan(60.0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_partial_fill_stops_before_stale_moc() {
        assert_cancel_requires_replan(20.0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_without_fill_still_requires_replan() {
        assert_cancel_requires_replan(0.0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn cover_response_after_deadline_never_opens_new_leg() {
        let broker = FakeBroker::new(
            -100.0,
            Vec::new(),
            vec![Event::Position {
                response_delay: Duration::from_secs(1),
                qty: 0.0,
            }],
        );

        let actions = vec![
            Action::Submit(OrderSpec {
                symbol: "SPY".to_owned(),
                qty: 100.0,
                tif: Tif::Day,
                client_order_id: "mrd-cover".to_owned(),
            }),
            Action::AwaitPosition {
                symbol: "SPY".to_owned(),
                qty: 0.0,
            },
            Action::Submit(OrderSpec {
                symbol: "SPY".to_owned(),
                qty: 50.0,
                tif: Tif::Cls,
                client_order_id: "mrd-opening".to_owned(),
            }),
        ];

        let start = tokio::time::Instant::now();
        let deadline =
            (start + Duration::from_millis(1500)).into_std();

        let error = apply(&broker, &actions, deadline)
            .await
            .expect_err("the phase must expire during the position RPC");

        assert!(error.contains("deadline"), "{error}");

        let submitted = broker.submissions();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].client_order_id, "mrd-cover");
        assert_eq!(submitted[0].qty, 100.0);

        // The cover filled, but the late response must not authorize
        // the opening leg.
        assert_eq!(broker.position_now(), 0.0);

        assert!(
            tokio::time::Instant::now().duration_since(start)
                < Duration::from_secs(2),
            "must not wait for the late position response"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expired_deadline_never_calls_submit() {
        let broker = FakeBroker::new(
            0.0,
            Vec::new(),
            Vec::new(),
        );

        let actions = vec![Action::Submit(OrderSpec {
            symbol: "SPY".to_owned(),
            qty: 50.0,
            tif: Tif::Day,
            client_order_id: "mrd-too-late".to_owned(),
        })];

        let deadline = tokio::time::Instant::now().into_std();

        let error = apply(&broker, &actions, deadline)
            .await
            .expect_err("an expired phase must reject Submit");

        assert!(error.contains("deadline"), "{error}");
        assert!(broker.submissions().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_confirmation_is_bounded_by_phase_deadline() {
        let broker = FakeBroker::new(
            0.0,
            vec![day_order(60.0)],
            cancel_events(0.0),
        );
        let actions = cancel_then_stale_moc();

        let start = tokio::time::Instant::now();
        let deadline =
            (start + Duration::from_millis(500)).into_std();

        let error = apply(&broker, &actions, deadline)
            .await
            .expect_err("cancel confirmation must respect the phase");

        assert!(error.contains("deadline"), "{error}");
        assert_eq!(
            broker.cancellations(),
            vec!["day-1".to_owned()]
        );
        assert!(broker.submissions().is_empty());

        assert!(
            tokio::time::Instant::now().duration_since(start)
                < Duration::from_secs(1),
            "must not consume the original one-second polling wait"
        );
    }
}

/// Claude's independent injection tests (round 4 scoring, item 4): the
/// fill-during-cancel race end to end — `apply` must stop with `Replan`,
/// and a re-plan from the *fresh* broker state must size the MOC on the
/// filled position (+40), never on the stale one (+100).
mod claude_injection_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Scripted open_orders responses; position is mutated when a fill is
    /// injected. cancel() always acks; submit() records.
    struct Fake {
        position: Mutex<f64>,
        open_script: Mutex<VecDeque<(f64, Vec<BrokerOrder>)>>, // (fill to inject, open list after)
        submitted: Mutex<Vec<OrderSpec>>,
    }

    impl BrokerApi for Fake {
        async fn position(&self, _s: &str) -> Result<f64, String> {
            Ok(*self.position.lock().expect("lock"))
        }
        async fn open_orders(&self, _s: &str) -> Result<Vec<BrokerOrder>, String> {
            let (fill, open) = self.open_script.lock().expect("lock").pop_front().ok_or("script exhausted")?;
            *self.position.lock().expect("lock") += fill;
            Ok(open)
        }
        async fn submit(&self, spec: &OrderSpec) -> Result<String, String> {
            self.submitted.lock().expect("lock").push(spec.clone());
            Ok("fake".into())
        }
        async fn cancel(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn day60() -> BrokerOrder {
        BrokerOrder { id: "D1".into(), client_order_id: "mrd-2026-09-16-SPY-376-0".into(), symbol: "SPY".into(), qty: 60.0, tif: Tif::Day, status: "new".into() }
    }

    /// ClosePreview with broker 0, Day +60 in flight, B wants +100:
    /// plan cancels the Day and (stale) submits MOC +100.
    fn close_preview_plan() -> (Vec<Action>, f64) {
        let open = [day60()];
        let i = input(Phase::ClosePreview, 100.0, 0.0, 100.0, 0.0, &open);
        let actions = plan(&i);
        let stale_moc = actions.iter().find_map(|a| match a {
            Action::Submit(s) if s.tif == Tif::Cls => Some(s.qty),
            _ => None,
        });
        (actions, stale_moc.expect("old plan carries a stale MOC"))
    }

    #[tokio::test(start_paused = true)]
    async fn fill_during_cancel_then_replan_sizes_the_moc_on_the_filled_position() {
        let (actions, stale) = close_preview_plan();
        assert_eq!(stale, 100.0, "the plan as computed would send +100");
        // first open_orders poll: still open (no fill yet); second: filled +60 and gone
        let fake = Fake {
            position: Mutex::new(0.0),
            open_script: Mutex::new(VecDeque::from(vec![(0.0, vec![day60()]), (60.0, vec![])])),
            submitted: Mutex::new(Vec::new()),
        };
        let deadline = (tokio::time::Instant::now() + Duration::from_secs(60)).into_std();
        let (_, outcome) = apply(&fake, &actions, deadline).await.expect("apply ok");
        assert_eq!(outcome, Outcome::Replan);
        assert!(fake.submitted.lock().expect("lock").is_empty(), "stale +100 MOC must not be sent");
        // the driver re-plans from broker truth at the next poll:
        let pos = *fake.position.lock().expect("lock");
        assert_eq!(pos, 60.0);
        let fresh = input(Phase::ClosePreview, 100.0, 0.0, 100.0, pos, &[]);
        let again = plan(&fresh);
        let moc: Vec<f64> = again.iter().filter_map(|a| match a { Action::Submit(s) if s.tif == Tif::Cls => Some(s.qty), _ => None }).collect();
        assert_eq!(moc, vec![40.0], "re-plan sizes the MOC on the filled position");
        // and the overnight book would be exactly B's target
        assert_eq!(pos + moc[0], 100.0);
    }

    #[tokio::test(start_paused = true)]
    async fn partial_fill_during_cancel_is_also_absorbed_by_the_replan() {
        let (actions, _) = close_preview_plan();
        let fake = Fake {
            position: Mutex::new(0.0),
            open_script: Mutex::new(VecDeque::from(vec![(25.0, vec![])])),
            submitted: Mutex::new(Vec::new()),
        };
        let deadline = (tokio::time::Instant::now() + Duration::from_secs(60)).into_std();
        let (_, outcome) = apply(&fake, &actions, deadline).await.expect("apply ok");
        assert_eq!(outcome, Outcome::Replan);
        assert!(fake.submitted.lock().expect("lock").is_empty());
        let fresh = input(Phase::ClosePreview, 100.0, 0.0, 100.0, 25.0, &[]);
        let moc: Vec<f64> = plan(&fresh).iter().filter_map(|a| match a { Action::Submit(s) if s.tif == Tif::Cls => Some(s.qty), _ => None }).collect();
        assert_eq!(moc, vec![75.0]);
    }
}
