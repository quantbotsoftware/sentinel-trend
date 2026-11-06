use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use tauri::{AppHandle, Manager};
use tokio::time::Duration;

use crate::state::TitanState;

const WATCHDOG_INTERVAL_SECS: u64 = 30;
const STALE_WARN_SECS: i64 = 60;
const STALE_DEAD_SECS: i64 = 120;
const STATUS_EMIT_INTERVAL: u32 = 10; // every 10 ticks = 5 min

#[derive(Debug, Clone, Serialize)]
struct WatchdogAlert {
    loop_name: String,
    stale_secs: i64,
    severity: String,
}

#[derive(Debug, Clone, Serialize)]
struct WatchdogStatus {
    loops_healthy: u32,
    loops_warning: u32,
    loops_dead: u32,
    details: Vec<LoopDetail>,
}

#[derive(Debug, Clone, Serialize)]
struct LoopDetail {
    name: String,
    running: bool,
    last_tick_secs_ago: i64,
    tick_count: u64,
    status: String,
}

pub async fn run_watchdog(state: Arc<TitanState>, app: AppHandle) {
    tracing::info!(component = "watchdog", "Watchdog loop started");

    let mut ticker = tokio::time::interval(Duration::from_secs(WATCHDOG_INTERVAL_SECS));
    let mut check_count: u32 = 0;

    loop {
        ticker.tick().await;

        if state.shutdown.is_cancelled() {
            break;
        }

        {
            let mut sys = state.system.lock().unwrap();
            if let Some(h) = sys.loops.get_mut("watchdog") {
                h.tick();
            }
        }

        check_count += 1;
        let now = Utc::now();

        let sys = state.system.lock().unwrap();
        let mut healthy: u32 = 0;
        let mut warning: u32 = 0;
        let mut dead: u32 = 0;
        let mut details = Vec::new();

        for (name, health) in &sys.loops {
            if name == "watchdog" {
                healthy += 1;
                details.push(LoopDetail {
                    name: name.clone(),
                    running: true,
                    last_tick_secs_ago: 0,
                    tick_count: health.tick_count,
                    status: "OK".into(),
                });
                continue;
            }

            let age_secs = (now - health.last_tick).num_seconds();

            if !health.running || health.tick_count == 0 {
                if age_secs > STALE_DEAD_SECS {
                    dead += 1;
                    details.push(LoopDetail {
                        name: name.clone(),
                        running: false,
                        last_tick_secs_ago: age_secs,
                        tick_count: health.tick_count,
                        status: "DEAD".into(),
                    });
                } else {
                    details.push(LoopDetail {
                        name: name.clone(),
                        running: false,
                        last_tick_secs_ago: age_secs,
                        tick_count: health.tick_count,
                        status: "STARTING".into(),
                    });
                }
                continue;
            }

            let (status_str, bucket) = if age_secs >= STALE_DEAD_SECS {
                ("DEAD", &mut dead)
            } else if age_secs >= STALE_WARN_SECS {
                ("STALE", &mut warning)
            } else {
                ("OK", &mut healthy)
            };
            *bucket += 1;

            details.push(LoopDetail {
                name: name.clone(),
                running: health.running,
                last_tick_secs_ago: age_secs,
                tick_count: health.tick_count,
                status: status_str.into(),
            });

            if age_secs >= STALE_DEAD_SECS {
                tracing::error!(
                    component = "watchdog",
                    loop_name = %name,
                    stale_secs = age_secs,
                    "Loop appears DEAD (no tick for {age_secs}s)"
                );
                let _ = app.emit_all(
                    "watchdog-alert",
                    WatchdogAlert {
                        loop_name: name.clone(),
                        stale_secs: age_secs,
                        severity: "DEAD".into(),
                    },
                );
            } else if age_secs >= STALE_WARN_SECS {
                tracing::warn!(
                    component = "watchdog",
                    loop_name = %name,
                    stale_secs = age_secs,
                    "Loop stale (no tick for {age_secs}s)"
                );
                let _ = app.emit_all(
                    "watchdog-alert",
                    WatchdogAlert {
                        loop_name: name.clone(),
                        stale_secs: age_secs,
                        severity: "STALE".into(),
                    },
                );
            }
        }

        drop(sys);

        if check_count.is_multiple_of(STATUS_EMIT_INTERVAL) {
            let _ = app.emit_all(
                "watchdog-status",
                WatchdogStatus {
                    loops_healthy: healthy,
                    loops_warning: warning,
                    loops_dead: dead,
                    details,
                },
            );
        }
    }

    tracing::info!(component = "watchdog", "Watchdog loop exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use crate::state::LoopHealth;

    #[test]
    fn stale_detection_thresholds() {
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut sys = state.system.lock().unwrap();
            sys.register_loop("test_loop");
            if let Some(h) = sys.loops.get_mut("test_loop") {
                h.tick();
            }
        }

        let sys = state.system.lock().unwrap();
        let h = sys.loops.get("test_loop").unwrap();
        let age = (Utc::now() - h.last_tick).num_seconds();
        assert!(age < STALE_WARN_SECS);
    }

    #[test]
    fn never_ticked_loop_detected() {
        let health = LoopHealth::new();
        assert!(!health.running);
        assert_eq!(health.tick_count, 0);
    }

    #[test]
    fn loop_detail_serializes() {
        let d = LoopDetail {
            name: "sentinel".into(),
            running: true,
            last_tick_secs_ago: 5,
            tick_count: 42,
            status: "OK".into(),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("sentinel"));
        assert!(json.contains("42"));
    }
}
