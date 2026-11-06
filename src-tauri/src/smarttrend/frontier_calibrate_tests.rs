use super::*;
use crate::smarttrend::strategies::frontier::FrontierThresholds;

/// Effective-threshold map for the default (no-override) config —
/// Frontier defaults plus APEX defaults, mirroring the command layer.
fn current_defaults() -> HashMap<String, f64> {
    let th = FrontierThresholds::default();
    let mut map: HashMap<String, f64> = CALIBRATION_BOUNDS
        .iter()
        .filter_map(|(k, _, _)| th.get(k).map(|v| ((*k).to_string(), v)))
        .collect();
    for (k, _, _) in APEX_CALIBRATION_BOUNDS {
        if let Some(v) = crate::smarttrend::strategies::apex_calibration::apex_default_for(k) {
            map.insert((*k).to_string(), v);
        }
    }
    map
}

fn cond(name: &str, passed: bool, value: f64) -> CondObs {
    CondObs {
        name: name.into(),
        passed,
        value,
        threshold: 0.0,
    }
}

fn near_miss_spread(day: &str, spread: f64) -> CalRow {
    CalRow {
        day: day.into(),
        setup: "tape_ignition".into(),
        conditions: vec![
            cond("micro_warmup", true, 300.0),
            cond("pps_15s", true, 3.0),
            cond("signed_notional_15s", true, 50_000.0),
            cond("spread_bps", false, spread),
            cond("imbalance", true, 0.4),
        ],
        is_candidate: false,
    }
}

fn candidate(day: &str) -> CalRow {
    CalRow {
        day: day.into(),
        setup: "tape_ignition".into(),
        conditions: vec![
            cond("pps_15s", true, 3.0),
            cond("spread_bps", true, 20.0),
        ],
        is_candidate: true,
    }
}

#[test]
fn in_target_band_yields_no_proposals() {
    // 5 candidates / 1 day = 5/day ∈ [3, 8].
    let rows: Vec<CalRow> = (0..5).map(|_| candidate("2026-08-11")).collect();
    let rep = calibrate(&rows, &current_defaults());
    assert!(rep.in_target);
    assert!(rep.proposals.is_empty());
    assert_eq!(rep.candidates, 5);
    assert_eq!(rep.days_observed, 1);
}

#[test]
fn low_rate_proposes_loosening_sole_blocker_median() {
    // 1 candidate + 4 near-misses blocked ONLY by spread (44/46/50/60
    // vs max 40) → rate 1/day < 3 → propose raising tape_max_spread_bps
    // to the median (48), converting the two ≤48 rows.
    let mut rows = vec![candidate("2026-08-11")];
    for s in [44.0, 46.0, 50.0, 60.0] {
        rows.push(near_miss_spread("2026-08-11", s));
    }
    let rep = calibrate(&rows, &current_defaults());
    assert!(!rep.in_target);
    let p = rep
        .proposals
        .iter()
        .find(|p| p.key == "tape_max_spread_bps")
        .expect("spread proposal");
    assert!(p.suggested > p.current, "loosening a max-type raises it");
    assert!((p.suggested - 48.0).abs() < 1e-9);
    assert_eq!(p.sole_blocked, 4);
    assert!((p.projected_extra_per_day - 2.0).abs() < 1e-9);
}

#[test]
fn multi_condition_near_misses_do_not_project() {
    // Near-miss failing TWO conditions is not a sole-blocker — no
    // single threshold move converts it, so no proposal may claim it.
    let mut row = near_miss_spread("2026-08-11", 44.0);
    row.conditions.push(cond("imbalance", false, 0.05));
    // Recompute: imbalance now failed too (replace the passing one).
    row.conditions.retain(|c| !(c.name == "imbalance" && c.passed));
    let rep = calibrate(&[row], &current_defaults());
    assert!(rep.proposals.is_empty());
}

#[test]
fn loosening_clamps_to_calibration_band() {
    // Median of blocking spreads (90) exceeds the band cap (80) →
    // suggestion clamps to 80; only the in-band row (60) converts.
    let rows: Vec<CalRow> = [60.0, 90.0, 100.0]
        .iter()
        .map(|s| near_miss_spread("2026-08-11", *s))
        .collect();
    let rep = calibrate(&rows, &current_defaults());
    let p = rep
        .proposals
        .iter()
        .find(|p| p.key == "tape_max_spread_bps")
        .expect("spread proposal");
    assert!((p.suggested - 80.0).abs() < 1e-9, "clamped to band hi");
    assert!((p.projected_extra_per_day - 1.0).abs() < 1e-9, "only in-band rows convert");
}

#[test]
fn out_of_band_blockers_yield_no_proposal() {
    // All blocking spreads above the band cap → even the max in-band
    // threshold converts nothing → the honest engine stays silent.
    let rows: Vec<CalRow> = [90.0, 95.0, 100.0]
        .iter()
        .map(|s| near_miss_spread("2026-08-11", *s))
        .collect();
    let rep = calibrate(&rows, &current_defaults());
    assert!(rep.proposals.is_empty());
}

#[test]
fn high_rate_proposes_tightening() {
    // 20 candidates in 1 day (rate 20 > 8) with spread values spread
    // out → tighten tape_max_spread_bps downward.
    let mut rows = Vec::new();
    for i in 0..20 {
        let mut c = candidate("2026-08-11");
        c.conditions = vec![cond("spread_bps", true, 10.0 + i as f64)];
        rows.push(c);
    }
    let rep = calibrate(&rows, &current_defaults());
    assert!(!rep.in_target);
    let p = rep
        .proposals
        .iter()
        .find(|p| p.key == "tape_max_spread_bps")
        .expect("tightening proposal");
    assert!(p.suggested < p.current, "tightening a max-type lowers it");
    assert!(p.suggested >= 15.0, "stays inside the calibration band");
}

#[test]
fn apex_sole_blocker_rvol_calm_proposes_loosening() {
    // APEX rows share the protocol: near-misses blocked ONLY by
    // rvol_calm (values 1.9/2.0/2.1 vs default threshold 2.2) at a
    // sub-target rate → propose lowering apex_rvol_calm to the median
    // (2.0), clamped inside the apex band (1.5–4.0).
    let apex_miss = |rvol: f64| CalRow {
        day: "2026-08-11".into(),
        setup: "apex".into(),
        conditions: vec![
            cond("et_window", true, 600.0),
            cond("spread_bps", true, 15.0),
            cond("rvol_calm", false, rvol),
            cond("ignition_range", true, 1.4),
            cond("ignition_vol", true, 2.4),
        ],
        is_candidate: false,
    };
    let rows: Vec<CalRow> = [1.9, 2.0, 2.1].iter().map(|r| apex_miss(*r)).collect();
    let rep = calibrate(&rows, &current_defaults());
    let p = rep
        .proposals
        .iter()
        .find(|p| p.key == "apex_rvol_calm")
        .expect("apex rvol proposal");
    assert_eq!(p.setup, "apex");
    assert!((p.current - 2.2).abs() < 1e-9);
    assert!((p.suggested - 2.0).abs() < 1e-9, "median of blockers");
    assert_eq!(p.sole_blocked, 3);
}

#[test]
fn overrides_roundtrip_through_thresholds() {
    let mut ov = std::collections::HashMap::new();
    ov.insert("tape_max_spread_bps".to_string(), 55.0);
    ov.insert("resume_max_age_secs".to_string(), 120.0);
    ov.insert("tape_min_pps_15s".to_string(), 999.0); // clamps to 6.0
    ov.insert("unknown_key".to_string(), 1.0); // ignored
    ov.insert("sweep_min_gap_pct".to_string(), f64::NAN); // ignored
    let th = FrontierThresholds::with_overrides(&ov);
    assert!((th.tape_max_spread_bps - 55.0).abs() < 1e-9);
    assert_eq!(th.resume_max_age_secs, 120);
    assert!((th.tape_min_pps_15s - 6.0).abs() < 1e-9);
    assert!((th.sweep_min_gap_pct - 3.0).abs() < 1e-9, "NaN ignored → default");
}

#[test]
fn integer_second_thresholds_skip_subsecond_noops() {
    // Current micro_warmup is 302s. Sole-blocker ages cluster at
    // 301.4/301.5/301.6 → median 301.5 rounds to 302, which does not
    // loosen the u64 threshold. The engine must stay silent instead
    // of looping APPLY → same row forever.
    let miss = |age: f64| CalRow {
        day: "2026-08-11".into(),
        setup: "tape_ignition".into(),
        conditions: vec![
            cond("micro_warmup", false, age),
            cond("pps_15s", true, 3.0),
            cond("signed_notional_15s", true, 50_000.0),
            cond("spread_bps", true, 20.0),
            cond("imbalance", true, 0.4),
        ],
        is_candidate: false,
    };
    let mut current = current_defaults();
    current.insert("tape_min_micro_age_secs".to_string(), 302.0);
    let rows: Vec<CalRow> = [301.4, 301.5, 301.6].iter().map(|a| miss(*a)).collect();
    let rep = calibrate(&rows, &current);
    assert!(
        rep.proposals
            .iter()
            .all(|p| p.key != "tape_min_micro_age_secs"),
        "sub-second loosen must not become a proposal"
    );
}

#[test]
fn snap_threshold_rounds_second_keys_only() {
    assert!((snap_threshold("tape_min_micro_age_secs", 301.5) - 302.0).abs() < 1e-9);
    assert!((snap_threshold("resume_max_age_secs", 89.4) - 89.0).abs() < 1e-9);
    assert!((snap_threshold("tape_max_spread_bps", 40.4) - 40.4).abs() < 1e-9);
}
