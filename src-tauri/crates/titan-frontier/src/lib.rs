//! MERIDIAN frontier — research crate, deliberately **outside** the live
//! strategy hash (`crates/titan-meridian/src` + `src/meridian`).
//!
//! Layering (R4): `daily` (close panel) → `signal` (momentum, vol, weights)
//! → `sim` (weekly rebalance, costs) → `curve` (series arithmetic) →
//! `gate` (A+B+T combination and the pre-registered F1 gate).
//!
//! Every rule here is fixed in
//! `docs/meridian/validation/2026-09-18_sleeve_t_preregistration.md`; the
//! code implements it and adds no parameter that document does not name.
//! `grid` (Sleeve A on a shifted decision grid) follows
//! `docs/meridian/validation/2026-09-26_chat8_preregistration.md`;
//! `hurst_gate` (DFA Hurst entry gate around Sleeve A) follows
//! `docs/meridian/validation/2026-09-26_hurst_gate_preregistration.md`;
//! `regime_scale` (VIX term-structure A/B scaling) follows
//! `docs/meridian/validation/2026-09-26_vix_regime_preregistration.md`;
//! `r5` (Astra round 5: conditional overnight cap, sleeve scaling) follows
//! `docs/meridian/validation/2026-09-27_runda5_q2_q3_preregistration.md`.
//! No tokio, no tauri, no network: files in, Markdown out.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod causality;
pub mod curve;
pub mod daily;
pub mod diag;
pub mod gate;
pub mod grid;
pub mod hurst_gate;
pub mod r5;
pub mod regime_scale;
pub mod signal;
pub mod sim;

pub use curve::Curve;
pub use daily::{Panel, Series};
pub use gate::{evaluate, Combination, GateResult};
pub use signal::SignalParams;
pub use sim::{run, SimParams, TResult, Trade};
