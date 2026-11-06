pub mod engine;
pub mod orb;
pub mod premarket;
pub mod report;

pub use engine::{run, run_with, SimConfig, SimResult, SymbolData, TradeRecord};
pub use orb::{run_orb_multi, OrbConfig, OrbParams, OrbResult, OrbTrade, StopRule};
pub use premarket::{run_pm, run_pm_multi, ClassFilter, PmResult, PmSimConfig, PmTrade};
pub use report::{markdown_report, yearly_table, YearRow};
