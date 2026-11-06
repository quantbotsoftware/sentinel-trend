//! Headless MERIDIAN executor (ANALIZA chat 2 §4.1).
//!
//! `sentinel-trend --meridian-daemon` runs the driver with no window, under
//! the systemd user unit `tools/systemd/meridian-daemon.service`. Three
//! pieces make the GUI and the daemon coexist safely:
//!
//! * **Executor lease** — `<data_dir>/meridian/live/driver.lock`, an
//!   advisory `flock` the kernel releases when the holder dies. Exactly one
//!   process drives (syncs history, logs intents, sends orders).
//! * **State file** — the driver writes every `meridian-state` payload to
//!   `<data_dir>/meridian/live/state.json` (atomic rename).
//! * **Viewer** — a GUI that finds the lease taken re-emits the state file
//!   to the tab; if nobody refreshes the file for `TAKEOVER_AFTER` and the
//!   lease is free (daemon stopped, not restarted), the GUI takes over.
//!
//! Same binary, same config file, same twin log ⇒ same artefact hashes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;

use super::driver::{remember_state, run_meridian_driver};
use super::history;
use crate::state::TitanState;

const LOOP_NAME: &str = "meridian";
const VIEWER_POLL: Duration = Duration::from_secs(3);
const TAKEOVER_AFTER: Duration = Duration::from_secs(90);
const LEASE_RETRY: Duration = Duration::from_secs(10);
const CONFIG_RELOAD_EVERY: Duration = Duration::from_secs(20);

pub fn lease_path() -> PathBuf {
    history::cache_dir().join("driver.lock")
}

pub fn state_path() -> PathBuf {
    history::cache_dir().join("state.json")
}

/// Held by the single MERIDIAN executor on this machine. Dropping it (or
/// the process dying) releases the lock.
pub struct ExecutorLease {
    _file: std::fs::File,
}

impl ExecutorLease {
    pub fn try_acquire() -> std::io::Result<Option<Self>> {
        Self::try_acquire_at(&lease_path())
    }

    pub fn try_acquire_at(path: &Path) -> std::io::Result<Option<Self>> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path)?;
        match file.try_lock() {
            Ok(()) => {
                let _ = file.set_len(0);
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Some(Self { _file: file }))
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Atomic write of the latest `meridian-state` payload for viewers.
pub fn write_state(v: &serde_json::Value) {
    let path = state_path();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub fn read_state() -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(state_path()).ok()?).ok()
}

fn tick(state: &TitanState) {
    let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(h) = sys.loops.get_mut(LOOP_NAME) {
        h.tick();
    }
}

/// GUI entry point: drive if the lease is free, otherwise view.
pub async fn start_in_app(state: Arc<TitanState>, app: tauri::AppHandle) {
    match ExecutorLease::try_acquire() {
        Ok(Some(lease)) => run_meridian_driver(state, Some(app), Some(lease), "app").await,
        Ok(None) => run_viewer(state, app).await,
        Err(e) => {
            tracing::error!(component = LOOP_NAME, "executor lease unavailable ({e}) — driving without it");
            run_meridian_driver(state, Some(app), None, "app").await
        }
    }
}

/// Re-emit the daemon's state file to the tab; take over only when the
/// file has gone quiet and the lease is free (a stopped daemon).
async fn run_viewer(state: Arc<TitanState>, app: tauri::AppHandle) {
    tracing::info!(component = LOOP_NAME, "another process holds the MERIDIAN executor lease — this window is a viewer");
    let mut last_ts: Option<String> = None;
    let mut last_fresh = std::time::Instant::now();
    loop {
        if state.shutdown.is_cancelled() {
            return;
        }
        if let Some(v) = read_state() {
            let ts = v.get("ts").and_then(|t| t.as_str()).map(str::to_string);
            if ts != last_ts {
                last_ts = ts;
                last_fresh = std::time::Instant::now();
                remember_state(v.clone());
                let _ = app.emit_all("meridian-state", &v);
            }
        }
        tick(&state);
        if last_fresh.elapsed() >= TAKEOVER_AFTER {
            if let Ok(Some(lease)) = ExecutorLease::try_acquire() {
                tracing::warn!(component = LOOP_NAME, "no executor for {}s and the lease is free — this window takes over", TAKEOVER_AFTER.as_secs());
                return run_meridian_driver(state, Some(app), Some(lease), "app").await;
            }
            last_fresh = std::time::Instant::now();
        }
        tokio::select! {
            _ = tokio::time::sleep(VIEWER_POLL) => {}
            _ = state.shutdown.cancelled() => return,
        }
    }
}

/// Daemon side: wait for the lease (a GUI may be driving) unless shut down.
async fn acquire_blocking(state: &TitanState) -> Option<ExecutorLease> {
    let mut announced = false;
    loop {
        match ExecutorLease::try_acquire() {
            Ok(Some(l)) => return Some(l),
            Ok(None) if !announced => {
                announced = true;
                tracing::warn!(component = LOOP_NAME, "another process (the GUI?) holds the executor lease — waiting");
            }
            Ok(None) => {}
            Err(e) => tracing::error!(component = LOOP_NAME, "executor lease error: {e} — retrying"),
        }
        tokio::select! {
            _ = tokio::time::sleep(LEASE_RETRY) => {}
            _ = state.shutdown.cancelled() => return None,
        }
    }
}

/// The GUI saves `settings.json` atomically; pick up a changed file so a
/// toggle in the tab reaches the daemon without a restart. A file that does
/// not parse is ignored (never fall back to defaults while executing).
async fn reload_config_loop(state: Arc<TitanState>) {
    let path = crate::paths::config_dir().join("settings.json");
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut seen = mtime(&path);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(CONFIG_RELOAD_EVERY) => {}
            _ = state.shutdown.cancelled() => return,
        }
        let now = mtime(&path);
        if now == seen {
            continue;
        }
        let parsed = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<crate::config::RuntimeConfig>(&s).ok());
        let Some(fresh) = parsed else {
            tracing::warn!(component = LOOP_NAME, "settings.json changed but does not parse — keeping the running config");
            continue;
        };
        seen = now;
        let mut c = state.config.lock().unwrap_or_else(|p| p.into_inner());
        let changed = c.meridian != fresh.meridian || c.market.paper_mode != fresh.market.paper_mode || c.market.alpaca_key_id != fresh.market.alpaca_key_id;
        *c = fresh;
        drop(c);
        if changed {
            tracing::info!(component = LOOP_NAME, "config reloaded from settings.json (meridian / broker section changed)");
        }
    }
}

async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match term.as_mut() {
                Some(t) => { t.recv().await; }
                None => std::future::pending::<()>().await,
            }
        } => {}
    }
}

/// `sentinel-trend --meridian-daemon`: no window, no legacy loops.
pub fn run_headless() {
    crate::logging::init_named("meridian-daemon.log");
    tracing::info!(
        "MERIDIAN daemon v{} code {} strategy {} starting",
        env!("CARGO_PKG_VERSION"),
        env!("ST_GIT_HASH"),
        env!("ST_MERIDIAN_HASH"),
    );
    if let Err(e) = crate::paths::ensure_data_dirs() {
        tracing::error!("data directories: {e}");
    }
    let cfg = crate::ops::persistence::load_config();
    tracing::info!(paper = cfg.market.paper_mode, execute = cfg.meridian.execute, enabled = cfg.meridian.enabled, symbols = ?cfg.meridian.symbols, "config loaded");
    let state = Arc::new(TitanState::new(cfg));
    state.system.lock().unwrap_or_else(|p| p.into_inner()).register_loop(LOOP_NAME);
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    rt.block_on(async {
        let signal_state = Arc::clone(&state);
        tokio::spawn(async move {
            wait_for_signal().await;
            tracing::info!(component = LOOP_NAME, "signal received — shutting down");
            signal_state.shutdown.cancel();
        });
        let Some(lease) = acquire_blocking(&state).await else { return };
        let reload = tokio::spawn(reload_config_loop(Arc::clone(&state)));
        run_meridian_driver(Arc::clone(&state), None, Some(lease), "daemon").await;
        reload.abort();
    });
    tracing::info!("MERIDIAN daemon stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_exclusive_and_released_on_drop() {
        let dir = std::env::temp_dir().join(format!("mrd-lease-{}", std::process::id()));
        let path = dir.join("driver.lock");
        let first = ExecutorLease::try_acquire_at(&path).ok().flatten();
        assert!(first.is_some(), "first holder gets the lease");
        assert!(ExecutorLease::try_acquire_at(&path).ok().flatten().is_none(), "second contender is refused");
        drop(first);
        assert!(ExecutorLease::try_acquire_at(&path).ok().flatten().is_some(), "released on drop");
        let _ = std::fs::remove_dir_all(dir);
    }
}
