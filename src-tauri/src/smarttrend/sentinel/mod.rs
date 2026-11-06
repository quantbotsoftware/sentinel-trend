mod alerts;
mod dispatch;
mod inputs;
mod loop_run;
pub(crate) mod tournament_dispatch;

#[allow(unused_imports)]
pub use alerts::{MarkerPayload, SentinelAlert, SentinelMetrics};
#[allow(unused_imports)] // phase 6: loop no longer spawned; removed with the module in step 3
pub use loop_run::run_sentinel_loop;
