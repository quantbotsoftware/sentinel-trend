//! Strategy Lab — Rust backtest engine that exercises every
//! `Strategy` implementation (gap fade, ORB, stat-arb, …) over
//! historical Alpaca SIP bars with realistic fills and produces
//! a full performance report.
//!
//! ## Module layout
//!
//! - [`context_builder`] — reconstructs `MarketContext` from a bar
//!   slice; the single point of contact with the `Strategy` trait
//!   inputs, so live and backtest evaluation share semantics.
//! - [`fills`] — slippage / commission / intrabar SL+TP resolution
//!   plus limit-order cross detection.
//! - [`engine`] — pure simulator: one (strategy, symbol) at a time,
//!   produces a `SimReport` (trade ledger + equity curve).
//! - [`metrics`] — Sharpe / Sortino / drawdown / expectancy / CAGR /
//!   profit factor / exposure from a `SimReport`.
//! - [`runner`] — top-level orchestrator: serial Alpaca fetch (with
//!   gzip cache) → rayon-parallel simulations → per-strategy
//!   aggregation.
//!
//! All sub-modules are pure or pure-with-HTTP-async; nothing touches
//! Tauri state directly. The Tauri command (`run_strategy_backtest`)
//! lives in `commands::backtest_engine` and is the thin shim that
//! reads config, calls [`runner::run`], and emits the result.

pub mod apex_sweep;
pub mod cli;
pub mod context_builder;
pub mod engine;
pub mod fills;
pub mod metrics;
pub mod runner;
