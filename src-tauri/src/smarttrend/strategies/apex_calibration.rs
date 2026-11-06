//! F4-APEX — calibration surface for the house strategy.
//!
//! Bounds, defaults and the override-merge helper live here (sibling
//! of `apex.rs`, split to respect the R2 line cap). The evaluation
//! itself stays in `apex.rs`; these items are consumed by the
//! calibration engine (`frontier_calibrate`), the Tauri command layer
//! and the two `MarketContext` builders (live dispatch + trade plan).

use super::apex::Apex;
use super::StrategyId;

/// F4-APEX — calibratable threshold keys, prefixed `apex_` so they share
/// the `evolution_protocol.frontier_overrides` map with Frontier without
/// collisions. `(key, lo, hi)` — proposals are clamped into these bands.
pub const APEX_CALIBRATION_BOUNDS: &[(&str, f64, f64)] = &[
    ("apex_rvol_trending", 1.2, 3.5),
    ("apex_rvol_calm", 1.5, 4.0),
    ("apex_rvol_volatile", 2.0, 5.0),
    ("apex_max_spread_bps", 20.0, 150.0),
    ("apex_ignition_range_atr", 0.6, 2.5),
    ("apex_ignition_close_pos", 0.50, 0.85),
    ("apex_ignition_vol_mult", 0.8, 3.0),
    ("apex_max_extension_atr", 2.0, 20.0),
];

/// Default value for a calibratable APEX key (mirrors `Apex::default()`
/// plus the S29.3 regime-RVOL constants).
pub fn apex_default_for(key: &str) -> Option<f64> {
    let d = Apex::default();
    Some(match key {
        "apex_rvol_trending" => 1.8,
        "apex_rvol_calm" => 2.2,
        "apex_rvol_volatile" => 3.0,
        "apex_max_spread_bps" => d.max_spread_bps,
        "apex_ignition_range_atr" => d.ignition_range_atr,
        "apex_ignition_close_pos" => d.ignition_close_pos,
        "apex_ignition_vol_mult" => d.ignition_vol_mult,
        "apex_max_extension_atr" => d.max_extension_atr,
        _ => return None,
    })
}

/// Merge operator calibration overrides (the `apex_*` entries of
/// `evolution_protocol.frontier_overrides`) into a `MarketContext`
/// overrides map, stripped of the prefix so `get_param` finds them.
/// Applied AFTER the D+ snapshot: an explicit operator APPLY beats
/// automated tuning.
pub fn merge_apex_overrides(
    dst: &mut std::collections::HashMap<StrategyId, std::collections::HashMap<String, f64>>,
    cfg_overrides: &std::collections::HashMap<String, f64>,
) {
    for (k, v) in cfg_overrides {
        let Some(stripped) = k.strip_prefix("apex_") else {
            continue;
        };
        if !APEX_CALIBRATION_BOUNDS.iter().any(|(key, _, _)| *key == k) {
            continue;
        }
        dst.entry(StrategyId::Apex)
            .or_default()
            .insert(stripped.to_string(), *v);
    }
}
