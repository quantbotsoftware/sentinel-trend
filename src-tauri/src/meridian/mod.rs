//! MERIDIAN host integration (plan §4.5). The strategy itself lives in the
//! pure `titan-meridian` crate; this module only supplies data (history
//! cache), runs the replay and reports intents. R1: no imports from
//! `smarttrend/`; communication with the rest of the app goes through
//! `TitanState.config` and Tauri events.

pub mod broker;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod driver;
pub mod exec;
pub mod exec_apply;
pub mod history;
pub mod live_risk;
pub mod shadow;
pub mod twin_log;

pub use config::MeridianConfig;
#[allow(unused_imports)]
pub use exec::{Action, Phase};
pub use daemon::{run_headless, start_in_app};
pub use driver::last_state_json;

/// Last `meridian-state` payload (None until the first replay).
#[tauri::command]
pub fn get_meridian_state() -> Option<serde_json::Value> {
    last_state_json()
}
