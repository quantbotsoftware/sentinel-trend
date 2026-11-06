//! MERIDIAN C-PM end-to-end (in-binary, plan §4.9 phase 4): synthetic
//! SIP prints → `PremarketTape` → 1-min bars → `PmLiveBook` → signals.
//! Mirrors the 1 s cadence of `premarket_live` without sockets or Tauri.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use titan_meridian::catalyst::{classify, CatalystClass};
use titan_meridian::session::ext_index;
use titan_meridian::sleeves::{ExitReason, PmParams};

use super::premarket_book::{Headline, PmLiveBook, PmSignal, SignalKind};
use super::premarket_tape::PremarketTape;

/// 2026-09-03 is EDT: ET = UTC − 4 h.
fn et(h: u32, m: u32, s: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 3, h + 4, m, s).single().unwrap()
}

/// One synthetic print: (time, symbol, price, size).
type Print = (DateTime<Utc>, &'static str, f64, f64);

fn run(prints: &[Print], headline: (&str, DateTime<Utc>, &str, f64), until: DateTime<Utc>) -> (Vec<PmSignal>, PmLiveBook) {
    let mut tape = PremarketTape::new();
    let mut book = PmLiveBook::new(PmParams::default(), 100_000.0);
    let (sym, h_time, text, prev_close) = headline;
    let mut signals = Vec::new();
    let mut prints = prints.to_vec();
    prints.sort_by_key(|p| p.0);
    let mut i = 0;
    let mut armed = false;
    let mut t = prints.first().map(|p| p.0).unwrap_or(until);
    while t <= until {
        // headline arrives at its timestamp (classified like the live loop)
        if !armed && t >= h_time {
            let (date, idx) = ext_index(h_time).unwrap();
            let class = classify(text);
            book.on_headline(&Headline { date, idx, symbol: sym.into(), class, headline: text.into(), url: String::new(), prev_close });
            armed = true;
        }
        while i < prints.len() && prints[i].0 <= t {
            let (pt, s, px, sz) = prints[i];
            tape.on_trade(s, px, sz, pt);
            i += 1;
        }
        // 1 s tick, exactly as premarket_live does it
        tape.close_stale(t);
        for s in book.armed_symbols() {
            let bars = tape.drain_bars(&s);
            if !bars.is_empty() {
                signals.extend(book.on_bars(&s, &bars));
            }
        }
        t += chrono::Duration::seconds(1);
    }
    (signals, book)
}

/// DLTH-like morning: flat at 3.00, headline 05:45:10, ignition 05:49,
/// gap up into 09:30, fade below VWAP at 09:33.
fn dlth_prints() -> Vec<Print> {
    let mut v: Vec<Print> = vec![
        (et(5, 40, 5), "DLTH", 3.00, 500.0),
        (et(5, 44, 30), "DLTH", 3.01, 300.0),
        (et(5, 46, 10), "DLTH", 3.04, 5_000.0),
        (et(5, 47, 20), "DLTH", 3.18, 20_000.0),
        (et(5, 48, 5), "DLTH", 3.25, 30_000.0),
    ];
    // 05:49: 300 prints, 3.30 → 3.45, 400 sh each ⇒ ≈ $405k this minute
    for k in 0..300u32 {
        v.push((et(5, 49, 0) + chrono::Duration::milliseconds(k as i64 * 190), "DLTH", 3.30 + 0.15 * k as f64 / 299.0, 400.0));
    }
    v.push((et(5, 50, 1), "DLTH", 3.46, 1_000.0));
    v.push((et(5, 55, 0), "DLTH", 3.50, 2_000.0));
    // RTH open 09:30 at 3.70, then 09:31 3.72, 09:32 3.73 (holds above
    // VWAP ≈ 3.71), 09:33 3.40 (below VWAP ⇒ trail exit)
    v.push((et(9, 30, 0), "DLTH", 3.70, 50_000.0));
    v.push((et(9, 30, 30), "DLTH", 3.72, 50_000.0));
    v.push((et(9, 31, 5), "DLTH", 3.72, 30_000.0));
    v.push((et(9, 32, 5), "DLTH", 3.73, 30_000.0));
    v.push((et(9, 33, 5), "DLTH", 3.40, 30_000.0));
    // noise symbol with no headline — must never signal
    for k in 0..50u32 {
        v.push((et(5, 49, 0) + chrono::Duration::seconds(k as i64), "NOIS", 10.0 + k as f64, 10_000.0));
    }
    v
}

#[test]
fn headline_then_ignition_produces_entry_partial_and_trail_exit_at_the_right_minutes() {
    let headline = ("DLTH", et(5, 45, 10), "Duluth Holdings Q2 EPS $0.50 Beats $(0.01) Estimate, Sales $150M Beat $140M Estimate", 3.00);
    let (signals, book) = run(&dlth_prints(), headline, et(9, 40, 0));
    let kinds: Vec<(usize, SignalKind)> = signals.iter().map(|s| (s.bar_idx, s.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            (109, SignalKind::Entry),                          // 05:49 bar closes at 05:50 → ignition
            (330, SignalKind::Exit(ExitReason::OpenPartial)),  // 09:30 opening print
            (333, SignalKind::Exit(ExitReason::VwapTrail)),    // 09:33 close < VWAP
        ],
        "{signals:#?}"
    );
    let e = &signals[0];
    assert_eq!(e.class, CatalystClass::EarningsBeat);
    assert_eq!(e.event_idx, 105);
    assert!(e.change_pct >= 14.9 && e.change_pct <= 15.1, "close 3.45 vs 3.00 = +15 %");
    assert!(e.cum_dollar > 400_000.0);
    assert!(e.shares >= 1.0);
    assert!(signals.iter().all(|s| s.symbol == "DLTH"), "the un-catalysed mover never signals");
    let net: f64 = signals.iter().map(|s| s.shares).sum();
    assert_eq!(net, 0.0, "flat after the trail exit");
    assert!(book.armed_symbols().is_empty());
}

#[test]
fn headline_that_reports_an_existing_move_is_not_chased() {
    // Same tape, but the headline arrives at 05:50:30 — after the move.
    let headline = ("DLTH", et(5, 50, 30), "Duluth Holdings Q2 EPS $0.50 Beats $(0.01) Estimate, Sales $150M Beat $140M Estimate", 3.00);
    let (signals, _) = run(&dlth_prints(), headline, et(9, 40, 0));
    assert!(signals.is_empty(), "post-headline move is only +1.4 % ⇒ no ignition: {signals:?}");
}

#[test]
fn offering_headline_disarms_before_ignition() {
    let mut tape = PremarketTape::new();
    let mut book = PmLiveBook::new(PmParams::default(), 100_000.0);
    let date = NaiveDate::from_ymd_opt(2026, 9, 3).unwrap();
    let hl = |idx: usize, text: &str| Headline { date, idx, symbol: "DLTH".into(), class: classify(text), headline: text.into(), url: String::new(), prev_close: 3.0 };
    assert_eq!(book.on_headline(&hl(105, "Duluth Wins $50M Contract")), "armed");
    assert_eq!(book.on_headline(&hl(107, "Duluth Announces $20M Registered Direct Offering")), "disarmed");
    for p in dlth_prints() {
        tape.on_trade(p.1, p.2, p.3, p.0);
    }
    tape.close_stale(et(10, 30, 0));
    let bars = tape.drain_bars("DLTH");
    assert!(!bars.is_empty());
    assert!(book.on_bars("DLTH", &bars).is_empty());
    let px: HashMap<String, f64> = HashMap::new();
    assert!(book.flatten_all(390, &px).is_empty());
}
