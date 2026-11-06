//! MERIDIAN — pure strategy engine.
//!
//! Layering (R4): `bars`/`session` (data model) → `envelope`/`vwap`/`vol`
//! (primitives) → `sleeves` (signals → targets) → `netting`/`risk`/`spc`
//! (portfolio) → `sim` (historical simulation) → `stats` (validation).
//!
//! No tokio, no tauri, no network. `data::store` reads local archives only.
//! Money in simulation is `f64`; live execution converts to `Decimal` at
//! the `OrderIntent` boundary (S08) in the host application.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
// Tests may unwrap fixtures; production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bars;
pub mod catalyst;
pub mod data;
pub mod envelope;
pub mod netting;
pub mod risk;
pub mod session;
pub mod sim;
pub mod sleeve;
pub mod sleeves;
pub mod spc;
pub mod stats;
pub mod vol;
pub mod vwap;

pub use bars::{Bar, DaySeries};
pub use sleeve::{Sleeve, SleeveId, Target};
