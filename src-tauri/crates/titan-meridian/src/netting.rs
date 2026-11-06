//! Netting engine: per-sleeve targets → one net share position per symbol
//! → delta vs. current holding, with a rebalance band that suppresses
//! economically meaningless churn.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::sleeve::{SleeveId, Target};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NettingParams {
    /// Ignore deltas below this notional (USD).
    pub min_delta_usd: f64,
    /// Ignore deltas below this fraction of the target notional.
    pub min_delta_frac: f64,
}

impl Default for NettingParams {
    fn default() -> Self {
        Self { min_delta_usd: 200.0, min_delta_frac: 0.05 }
    }
}

/// Current targets of every sleeve member for one symbol (latest wins per
/// `(sleeve, variant)`).
#[derive(Debug, Default, Clone)]
pub struct SymbolBook {
    pub targets: BTreeMap<(SleeveId, u8), Target>,
}

impl SymbolBook {
    pub fn set(&mut self, t: Target) {
        self.targets.insert((t.sleeve, t.variant), t);
    }

    /// Current exposure of one ensemble member (0 when it has no target).
    pub fn exposure_of(&self, sleeve: SleeveId, variant: u8) -> f64 {
        self.targets.get(&(sleeve, variant)).map(|t| t.exposure).unwrap_or(0.0)
    }

    /// Net exposure of `sleeve` in this symbol from every member except
    /// `variant` (the room the risk cap leaves to that member).
    pub fn sleeve_exposure_excluding(&self, sleeve: SleeveId, variant: u8) -> f64 {
        self.targets.iter().filter(|((s, v), _)| *s == sleeve && *v != variant).map(|(_, t)| t.exposure).sum()
    }

    /// Members of `sleeve` with a non-zero exposure (mutable, e.g. to flatten).
    pub fn members_mut(&mut self, sleeve: SleeveId) -> impl Iterator<Item = &mut Target> {
        self.targets.iter_mut().filter(move |((s, _), t)| *s == sleeve && t.exposure != 0.0).map(|(_, t)| t)
    }

    /// Shares each sleeve wants (signed), summed over its members, each
    /// member sized on its own price_ref.
    pub fn sleeve_shares(&self, equity: f64) -> BTreeMap<SleeveId, f64> {
        let mut out: BTreeMap<SleeveId, f64> = BTreeMap::new();
        for ((id, _), t) in &self.targets {
            *out.entry(*id).or_default() += shares_for(equity, t.exposure, t.price_ref);
        }
        out
    }

    /// Net desired shares across sleeves.
    pub fn net_shares(&self, equity: f64) -> f64 {
        self.sleeve_shares(equity).values().sum()
    }
}

/// Signed whole shares: floor(|equity·exposure| / price) with the sign of exposure.
pub fn shares_for(equity: f64, exposure: f64, price_ref: f64) -> f64 {
    if price_ref <= 0.0 || price_ref.is_nan() || !exposure.is_finite() || exposure == 0.0 {
        return 0.0;
    }
    let q = (equity * exposure.abs() / price_ref).floor();
    if exposure < 0.0 {
        -q
    } else {
        q
    }
}

/// Delta to trade, or `None` if inside the rebalance band. Flattening
/// (target 0) is never suppressed.
pub fn delta_shares(current: f64, target: f64, price: f64, p: &NettingParams) -> Option<f64> {
    let d = target - current;
    if d == 0.0 {
        return None;
    }
    if target == 0.0 {
        return Some(d);
    }
    let notional = d.abs() * price;
    if notional < p.min_delta_usd || d.abs() < p.min_delta_frac * target.abs() {
        return None;
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sleeve::FillAt;

    fn t(s: SleeveId, e: f64, px: f64) -> Target {
        Target { sleeve: s, variant: 0, exposure: e, price_ref: px, fill_at: FillAt::Close }
    }

    #[test]
    fn long_plus_long_nets_to_sum_and_flip_nets_to_difference() {
        let mut b = SymbolBook::default();
        b.set(t(SleeveId::A, 2.0, 100.0));
        b.set(t(SleeveId::B, 1.0, 100.0));
        assert_eq!(b.net_shares(100_000.0), 3000.0);
        b.set(t(SleeveId::A, -1.0, 100.0));
        assert_eq!(b.net_shares(100_000.0), 0.0, "A short 1000 + B long 1000 ⇒ flat, zero cost");
        b.set(t(SleeveId::A, 0.0, 100.0));
        assert_eq!(b.net_shares(100_000.0), 1000.0);
    }

    #[test]
    fn ensemble_members_add_up_inside_their_sleeve() {
        let mut b = SymbolBook::default();
        b.set(Target { variant: 0, ..t(SleeveId::A, 0.5, 100.0) });
        b.set(Target { variant: 1, ..t(SleeveId::A, 0.5, 100.0) });
        b.set(Target { variant: 2, ..t(SleeveId::A, -0.5, 100.0) });
        b.set(t(SleeveId::B, 1.0, 100.0));
        let s = b.sleeve_shares(100_000.0);
        assert_eq!(s[&SleeveId::A], 500.0, "0.5 + 0.5 − 0.5 of $100k at $100");
        assert_eq!(s[&SleeveId::B], 1000.0);
        assert_eq!(b.net_shares(100_000.0), 1500.0);
        assert_eq!(b.exposure_of(SleeveId::A, 2), -0.5);
        assert_eq!(b.exposure_of(SleeveId::A, 7), 0.0);
        assert_eq!(b.sleeve_exposure_excluding(SleeveId::A, 2), 1.0, "the two longs, without the short member");
        assert_eq!(b.sleeve_exposure_excluding(SleeveId::A, 0), 0.0, "0.5 − 0.5");
        assert_eq!(b.sleeve_exposure_excluding(SleeveId::B, 0), 0.0, "B has no other member");
        for m in b.members_mut(SleeveId::A) {
            m.exposure = 0.0;
        }
        assert_eq!(b.sleeve_shares(100_000.0)[&SleeveId::A], 0.0);
        assert_eq!(b.net_shares(100_000.0), 1000.0, "B untouched");
    }

    #[test]
    fn shares_floor_and_sign() {
        assert_eq!(shares_for(100_000.0, 1.0, 333.0), 300.0);
        assert_eq!(shares_for(100_000.0, -0.5, 333.0), -150.0);
        assert_eq!(shares_for(100_000.0, 1.0, 0.0), 0.0);
        assert_eq!(shares_for(100_000.0, f64::NAN, 10.0), 0.0);
    }

    #[test]
    fn rebalance_band() {
        let p = NettingParams::default();
        assert_eq!(delta_shares(1000.0, 1000.0, 100.0, &p), None);
        assert_eq!(delta_shares(1000.0, 1001.0, 100.0, &p), None, "$100 < $200 band");
        assert_eq!(delta_shares(1000.0, 1030.0, 100.0, &p), None, "3 % < 5 % band");
        assert_eq!(delta_shares(1000.0, 1100.0, 100.0, &p), Some(100.0));
        assert_eq!(delta_shares(3.0, 0.0, 100.0, &p), Some(-3.0), "flatten is never suppressed");
        assert_eq!(delta_shares(0.0, 1.0, 100.0, &p), None, "1 share ≈ $100 stays inside the band");
    }
}
