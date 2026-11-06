mod types;
mod scoring;
mod prefilter;
mod snapshots;
mod run;
mod rvol;
mod sessions;
mod hot_news;
mod premarket;

#[allow(unused_imports)]
pub use premarket::{
    is_premarket_ignition, new_ignitions, IgnitionAlert, IgnitionParams,
    PREMARKET_IGNITION_PRESET,
};
pub use sessions::in_premarket;

#[allow(unused_imports)]
pub use types::{
    AlpacaAsset, ProgressReporter, ScanParams, ScanProgressEvent, ScanResult,
    ScoreInput, ScoreWeights, SnapshotDailyBar, SnapshotData,
};
#[allow(unused_imports)]
pub use scoring::{compute_candidate_score, score_weights_for_preset};
/// S13: shared RTH helper for the heuristic halt fallback in
/// `market::catalyst::is_halted_combined`.
///
/// S15.5: re-export point now lives in [`sessions`] (extracted to keep
/// `scoring.rs` under R2 cap when adding `in_premarket` / `in_extended`
/// / `compute_is_stale`). Caller import path unchanged.
pub(crate) use sessions::in_rth;
pub(crate) use sessions::{
    in_actionable_scan_window, is_current_session_trade, now_et, session_date_et_str,
    timestamp_is_today_et,
};
/// S14: helpers reused by `market::watchlist_refresh` to produce the
/// per-symbol metrics outside the scanner pipeline.
pub(crate) use scoring::extract_bar_metrics;
pub(crate) use rvol::time_adjusted_rvol;
/// S14: `LatestTrade` is needed by the `watchlist_refresh` test fixtures
/// to build snapshots with a fresh trade timestamp.
#[cfg(test)]
pub(crate) use types::LatestTrade;
#[allow(unused_imports)]
pub use prefilter::quant_prefilter;
#[allow(unused_imports)]
pub use rvol::RvolMethod;
#[allow(unused_imports)]
pub use snapshots::{
    fetch_active_assets, fetch_snapshots_batch, is_tradable_now, lookup_asset,
    refresh_asset_metadata,
};
pub use run::run_scan_with_progress;
