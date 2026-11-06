//! PRIME F1 — Decision Journal.
//!
//! Persistent, queryable audit trail of every entry DECISION the
//! system takes — including the ones that did NOT become trades.
//! Empirical motivation (audit 2026-07-03): the tracing logs show
//! 1,500–5,600 entry vetoes per day vs ~15 executed trades, and the
//! only record of *why* lives in rotated text logs (4.3 GB). Nobody
//! can answer "what did the spread gate cost us this week?" without
//! grep archaeology. This module closes that gap.
//!
//! ## Design
//!
//! - Storage: dedicated SQLite file `memory/decisions.sqlite` (WAL).
//!   Deliberately NOT the memory.sqlite connection: `MemoryDb` calls
//!   sit on the bot hot path and a second writer on the same
//!   connection-file pair would introduce SQLITE_BUSY contention.
//! - Writes: fire-and-forget. `log()` pushes into a bounded in-memory
//!   buffer (cap 4096, drop-oldest per S06); a dedicated writer thread
//!   flushes every 5 s inside one transaction. The hot path never
//!   touches disk.
//! - Retention: 30 days, pruned at startup and every 6 h by the
//!   writer thread (S06 cap + eviction).
//! - Reads: `query()` / `veto_summary()` open short-lived read
//!   connections so the UI never blocks the writer.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// Maximum buffered records between flushes. At the observed worst
/// case (~5,600 vetoes/day ≈ 4/min) this is >12h of headroom; the cap
/// exists for pathological loops, not normal operation.
const BUFFER_CAP: usize = 4096;
const FLUSH_INTERVAL_SECS: u64 = 5;
const RETENTION_DAYS: i64 = 30;
const PRUNE_INTERVAL_SECS: u64 = 6 * 3600;

// ════════════════════════════════════════════════════════════════════
//  Record
// ════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Unix seconds when the decision was made.
    pub ts: i64,
    pub symbol: String,
    pub timeframe: String,
    /// Signal source: a strategy id string ("gap_fade", …), "fsm" for
    /// the SmartTrend engine, or "prime" for fused decisions.
    pub source: String,
    /// "long" | "short" | "none" (when unknown at veto time).
    pub direction: String,
    /// Raw 0–100 score from the source (strategy score / FSM score).
    pub raw_score: f64,
    /// PRIME conviction 0..1 when fusion ran; `None` for decisions
    /// vetoed before fusion.
    pub conviction: Option<f64>,
    /// Decision lifecycle outcome:
    /// - `dispatched`: strategy signal entered the bot queue;
    /// - `entered`: broker confirmed at least a partial fill;
    /// - `execution_failed`: broker submitted but no shares filled;
    /// - `vetoed`: a gate rejected the signal before dispatch.
    ///
    /// Pre-S34 rows only contain `entered` / `vetoed`. `entered` was
    /// previously written at queue acceptance, which made queued APEX
    /// signals indistinguishable from real fills.
    pub outcome: String,
    /// Pipeline stage that produced the veto (e.g. "universe",
    /// "anti_chase", "quality_gate", "spread", "compliance",
    /// "conviction"). `None` for entered decisions.
    pub veto_stage: Option<String>,
    /// Human-readable veto reason (reuses existing SkipReason /
    /// diagnostic strings). `None` for entered decisions.
    pub veto_reason: Option<String>,
    /// Bar timestamp the decision was evaluated on (0 when unknown).
    pub bar_time: i64,
    /// Price at decision time (0.0 when unknown).
    pub price: f64,
    /// "calm" | "trending" | "volatile" | "panic" | "unknown".
    pub regime: String,
    /// P3 — JSON array of the 7 normalized fusion components
    /// `[signal, reliability, regime_fit, catalyst, rvol, spread,
    /// causal]` recorded when PRIME fused this decision. `None` for
    /// pre-fusion vetoes and legacy rows. Consumed by the
    /// Counterfactual Lab (`prime::counterfactual`).
    #[serde(default)]
    pub components: Option<String>,
}

impl DecisionRecord {
    /// Convenience constructor for a veto decision.
    #[allow(clippy::too_many_arguments)]
    pub fn vetoed(
        symbol: &str,
        timeframe: &str,
        source: &str,
        direction: &str,
        raw_score: f64,
        stage: &str,
        reason: &str,
        bar_time: i64,
        price: f64,
        regime: &str,
    ) -> Self {
        Self {
            ts: chrono::Utc::now().timestamp(),
            symbol: symbol.to_uppercase(),
            timeframe: timeframe.to_string(),
            source: source.to_string(),
            direction: direction.to_string(),
            raw_score,
            conviction: None,
            outcome: "vetoed".to_string(),
            veto_stage: Some(stage.to_string()),
            veto_reason: Some(reason.to_string()),
            bar_time,
            price,
            regime: regime.to_string(),
            components: None,
        }
    }

    /// Convenience constructor for an entered decision.
    #[allow(clippy::too_many_arguments)]
    pub fn entered(
        symbol: &str,
        timeframe: &str,
        source: &str,
        direction: &str,
        raw_score: f64,
        conviction: Option<f64>,
        bar_time: i64,
        price: f64,
        regime: &str,
    ) -> Self {
        Self {
            ts: chrono::Utc::now().timestamp(),
            symbol: symbol.to_uppercase(),
            timeframe: timeframe.to_string(),
            source: source.to_string(),
            direction: direction.to_string(),
            raw_score,
            conviction,
            outcome: "entered".to_string(),
            veto_stage: None,
            veto_reason: None,
            bar_time,
            price,
            regime: regime.to_string(),
            components: None,
        }
    }

    /// A strategy signal was accepted by the bot queue. This is not a
    /// broker fill; the bot writes `entered` only after the execution
    /// router confirms a non-zero filled quantity.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatched(
        symbol: &str,
        timeframe: &str,
        source: &str,
        direction: &str,
        raw_score: f64,
        conviction: Option<f64>,
        bar_time: i64,
        price: f64,
        regime: &str,
    ) -> Self {
        Self {
            ts: chrono::Utc::now().timestamp(),
            symbol: symbol.to_uppercase(),
            timeframe: timeframe.to_string(),
            source: source.to_string(),
            direction: direction.to_string(),
            raw_score,
            conviction,
            outcome: "dispatched".to_string(),
            veto_stage: None,
            veto_reason: None,
            bar_time,
            price,
            regime: regime.to_string(),
            components: None,
        }
    }

    /// The broker path ran but produced no filled quantity. Kept
    /// separate from a strategy veto: the signal may still be sound,
    /// while the failure belongs to execution quality / marketability.
    #[allow(clippy::too_many_arguments)]
    pub fn execution_failed(
        symbol: &str,
        timeframe: &str,
        source: &str,
        direction: &str,
        raw_score: f64,
        conviction: Option<f64>,
        bar_time: i64,
        price: f64,
        regime: &str,
        reason: &str,
    ) -> Self {
        Self {
            ts: chrono::Utc::now().timestamp(),
            symbol: symbol.to_uppercase(),
            timeframe: timeframe.to_string(),
            source: source.to_string(),
            direction: direction.to_string(),
            raw_score,
            conviction,
            outcome: "execution_failed".to_string(),
            veto_stage: Some("execution".to_string()),
            veto_reason: Some(reason.to_string()),
            bar_time,
            price,
            regime: regime.to_string(),
            components: None,
        }
    }

    /// P3 — attach the normalized fusion component vector (serialized
    /// as a JSON array) so the Counterfactual Lab can re-fuse this
    /// decision under alternative weights.
    pub fn with_components(mut self, norm: &[f64; 7]) -> Self {
        self.components = serde_json::to_string(norm).ok();
        self
    }

    /// P3 — record the fused conviction on a veto row (the `vetoed`
    /// constructor defaults it to `None` because most veto stages run
    /// before fusion; PRIME-stage vetoes DO have one).
    pub fn with_conviction(mut self, conviction: f64) -> Self {
        self.conviction = Some(conviction);
        self
    }

    /// P3 — parse the components JSON back into the fixed-order array.
    pub fn parsed_components(&self) -> Option<[f64; 7]> {
        self.components
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<f64>>(s).ok())
            .and_then(|v| <[f64; 7]>::try_from(v).ok())
    }

    /// P0 Asymmetry Protocol — a signal exit that was deferred because
    /// the position is a winner (`r` = R-multiple at deferral time).
    /// The PRIME ladder takes over management; this row makes the
    /// deferral measurable in the debrief ("did holding pay?").
    #[allow(clippy::too_many_arguments)]
    pub fn exit_deferred(
        symbol: &str,
        timeframe: &str,
        source: &str,
        direction: &str,
        r_multiple: f64,
        bar_time: i64,
        price: f64,
        regime: &str,
    ) -> Self {
        Self {
            ts: chrono::Utc::now().timestamp(),
            symbol: symbol.to_uppercase(),
            timeframe: timeframe.to_string(),
            source: source.to_string(),
            direction: direction.to_string(),
            // Store the deferral R-multiple in raw_score (score is
            // signal-domain; for exit rows the R at decision time is
            // the informative magnitude).
            raw_score: r_multiple,
            conviction: None,
            outcome: "exit_deferred".to_string(),
            veto_stage: Some("exit_arbitration".to_string()),
            veto_reason: Some(format!("winner_at_{r_multiple:.2}R")),
            bar_time,
            price,
            regime: regime.to_string(),
            components: None,
        }
    }
}

/// Aggregate row for the veto summary UI / shadow-trading prioritiser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VetoSummaryRow {
    pub veto_stage: String,
    pub veto_reason: String,
    pub count: i64,
}

// ════════════════════════════════════════════════════════════════════
//  DecisionLog
// ════════════════════════════════════════════════════════════════════

pub struct DecisionLog {
    buffer: Mutex<Vec<DecisionRecord>>,
    path: PathBuf,
    /// Set in tests to bypass the writer thread and flush inline.
    sync_mode: bool,
}

static GLOBAL: OnceLock<DecisionLog> = OnceLock::new();

impl DecisionLog {
    fn new(path: PathBuf, sync_mode: bool) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Open once here to run migrations + startup prune, then drop;
        // the writer thread / readers open their own connections.
        let conn = Self::open_conn(&path)?;
        prune_old(&conn, RETENTION_DAYS)?;
        drop(conn);
        Ok(Self {
            buffer: Mutex::new(Vec::new()),
            path,
            sync_mode,
        })
    }

    fn open_conn(path: &Path) -> rusqlite::Result<Connection> {
        let conn = Connection::open(path)?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        conn.busy_timeout(std::time::Duration::from_millis(2_000))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS decisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                symbol TEXT NOT NULL,
                timeframe TEXT NOT NULL,
                source TEXT NOT NULL,
                direction TEXT NOT NULL,
                raw_score REAL NOT NULL,
                conviction REAL,
                outcome TEXT NOT NULL,
                veto_stage TEXT,
                veto_reason TEXT,
                bar_time INTEGER NOT NULL,
                price REAL NOT NULL,
                regime TEXT NOT NULL,
                components TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_decisions_symbol_ts
                ON decisions(symbol, ts DESC);
             CREATE INDEX IF NOT EXISTS idx_decisions_ts
                ON decisions(ts DESC);
             CREATE INDEX IF NOT EXISTS idx_decisions_outcome_ts
                ON decisions(outcome, ts DESC);",
        )?;
        // P3 migration — databases created before the Counterfactual
        // Lab lack the `components` column. ALTER TABLE ADD COLUMN is
        // idempotent-by-error here: it fails harmlessly when the
        // column already exists (fresh DBs get it from CREATE TABLE).
        let _ = conn.execute("ALTER TABLE decisions ADD COLUMN components TEXT", []);
        Ok(conn)
    }

    /// Push a record into the buffer. Never blocks on I/O. Drops the
    /// OLDEST buffered record when at cap (S06 bounded-cache rule) —
    /// newest decisions are the ones the operator is debugging.
    pub fn log(&self, record: DecisionRecord) {
        {
            let mut buf = self.buffer.lock().unwrap_or_else(|p| p.into_inner());
            if buf.len() >= BUFFER_CAP {
                buf.remove(0);
            }
            buf.push(record);
        }
        if self.sync_mode {
            let _ = self.flush();
        }
    }

    /// Drain the buffer into SQLite inside one transaction. Called by
    /// the writer thread; public so tests / shutdown can force it.
    pub fn flush(&self) -> rusqlite::Result<usize> {
        let drained: Vec<DecisionRecord> = {
            let mut buf = self.buffer.lock().unwrap_or_else(|p| p.into_inner());
            if buf.is_empty() {
                return Ok(0);
            }
            std::mem::take(&mut *buf)
        };
        let mut conn = Self::open_conn(&self.path)?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO decisions (
                    ts, symbol, timeframe, source, direction, raw_score,
                    conviction, outcome, veto_stage, veto_reason,
                    bar_time, price, regime, components
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            )?;
            for r in &drained {
                stmt.execute(params![
                    r.ts,
                    r.symbol,
                    r.timeframe,
                    r.source,
                    r.direction,
                    r.raw_score,
                    r.conviction,
                    r.outcome,
                    r.veto_stage,
                    r.veto_reason,
                    r.bar_time,
                    r.price,
                    r.regime,
                    r.components,
                ])?;
            }
        }
        tx.commit()?;
        Ok(drained.len())
    }

    /// Most-recent decisions for `symbol` (all symbols when empty)
    /// within `[from_ts, to_ts]`. Newest first, capped at `limit`.
    pub fn query(
        &self,
        symbol: &str,
        from_ts: i64,
        to_ts: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<DecisionRecord>> {
        let conn = Self::open_conn(&self.path)?;
        let sym = symbol.to_uppercase();
        let mut stmt = conn.prepare(
            "SELECT ts, symbol, timeframe, source, direction, raw_score,
                    conviction, outcome, veto_stage, veto_reason,
                    bar_time, price, regime, components
               FROM decisions
              WHERE (?1 = '' OR symbol = ?1)
                AND ts BETWEEN ?2 AND ?3
              ORDER BY ts DESC, id DESC
              LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![sym, from_ts, to_ts, limit as i64], |r| {
            Ok(DecisionRecord {
                ts: r.get(0)?,
                symbol: r.get(1)?,
                timeframe: r.get(2)?,
                source: r.get(3)?,
                direction: r.get(4)?,
                raw_score: r.get(5)?,
                conviction: r.get(6)?,
                outcome: r.get(7)?,
                veto_stage: r.get(8)?,
                veto_reason: r.get(9)?,
                bar_time: r.get(10)?,
                price: r.get(11)?,
                regime: r.get(12)?,
                components: r.get(13)?,
            })
        })?;
        rows.collect()
    }

    /// P3 — all decisions since `from_ts` that carry a recorded fusion
    /// component vector (i.e. went through PRIME fusion). Oldest first
    /// so the Counterfactual Lab pairs them with outcomes
    /// chronologically. Capped at `limit`.
    pub fn rows_with_components(
        &self,
        from_ts: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<DecisionRecord>> {
        let conn = Self::open_conn(&self.path)?;
        let mut stmt = conn.prepare(
            "SELECT ts, symbol, timeframe, source, direction, raw_score,
                    conviction, outcome, veto_stage, veto_reason,
                    bar_time, price, regime, components
               FROM decisions
              WHERE ts >= ?1 AND components IS NOT NULL
              ORDER BY ts ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![from_ts, limit as i64], |r| {
            Ok(DecisionRecord {
                ts: r.get(0)?,
                symbol: r.get(1)?,
                timeframe: r.get(2)?,
                source: r.get(3)?,
                direction: r.get(4)?,
                raw_score: r.get(5)?,
                conviction: r.get(6)?,
                outcome: r.get(7)?,
                veto_stage: r.get(8)?,
                veto_reason: r.get(9)?,
                bar_time: r.get(10)?,
                price: r.get(11)?,
                regime: r.get(12)?,
                components: r.get(13)?,
            })
        })?;
        rows.collect()
    }

    /// Count of vetoes grouped by (stage, reason) since `from_ts`.
    pub fn veto_summary(&self, from_ts: i64) -> rusqlite::Result<Vec<VetoSummaryRow>> {
        let conn = Self::open_conn(&self.path)?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(veto_stage, ''), COALESCE(veto_reason, ''), COUNT(*)
               FROM decisions
              WHERE outcome = 'vetoed' AND ts >= ?1
              GROUP BY veto_stage, veto_reason
              ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map(params![from_ts], |r| {
            Ok(VetoSummaryRow {
                veto_stage: r.get(0)?,
                veto_reason: r.get(1)?,
                count: r.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Recent vetoed decisions that have NOT yet been claimed by the
    /// shadow-trading evaluator (Faza 5a reads through this).
    pub fn recent_vetoed(
        &self,
        since_ts: i64,
        limit: usize,
    ) -> rusqlite::Result<Vec<DecisionRecord>> {
        let conn = Self::open_conn(&self.path)?;
        let mut stmt = conn.prepare(
            "SELECT ts, symbol, timeframe, source, direction, raw_score,
                    conviction, outcome, veto_stage, veto_reason,
                    bar_time, price, regime, components
               FROM decisions
              WHERE outcome = 'vetoed' AND ts >= ?1 AND direction != 'none' AND price > 0.0
              ORDER BY ts ASC
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since_ts, limit as i64], |r| {
            Ok(DecisionRecord {
                ts: r.get(0)?,
                symbol: r.get(1)?,
                timeframe: r.get(2)?,
                source: r.get(3)?,
                direction: r.get(4)?,
                raw_score: r.get(5)?,
                conviction: r.get(6)?,
                outcome: r.get(7)?,
                veto_stage: r.get(8)?,
                veto_reason: r.get(9)?,
                bar_time: r.get(10)?,
                price: r.get(11)?,
                regime: r.get(12)?,
                components: r.get(13)?,
            })
        })?;
        rows.collect()
    }
}

fn prune_old(conn: &Connection, days: i64) -> rusqlite::Result<usize> {
    let cutoff = chrono::Utc::now().timestamp() - days * 86_400;
    conn.execute("DELETE FROM decisions WHERE ts < ?1", params![cutoff])
}

// ════════════════════════════════════════════════════════════════════
//  Global wiring
// ════════════════════════════════════════════════════════════════════

/// Initialise the global journal and spawn its writer thread. Called
/// once from `main.rs` after `ensure_data_dirs()`. Errors are logged
/// and swallowed — a broken decision journal must never stop trading.
pub fn init(path: PathBuf) {
    match DecisionLog::new(path, false) {
        Ok(log) => {
            if GLOBAL.set(log).is_err() {
                tracing::warn!(component = "decision_log", "init called twice — ignored");
                return;
            }
            std::thread::Builder::new()
                .name("decision-log-writer".into())
                .spawn(|| {
                    let mut last_prune = std::time::Instant::now();
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(FLUSH_INTERVAL_SECS));
                        let Some(log) = GLOBAL.get() else { return };
                        if let Err(e) = log.flush() {
                            tracing::warn!(component = "decision_log", error = %e, "flush failed");
                        }
                        if last_prune.elapsed().as_secs() >= PRUNE_INTERVAL_SECS {
                            last_prune = std::time::Instant::now();
                            if let Ok(conn) = DecisionLog::open_conn(&log.path) {
                                match prune_old(&conn, RETENTION_DAYS) {
                                    Ok(n) if n > 0 => tracing::info!(
                                        component = "decision_log",
                                        pruned = n,
                                        "retention prune"
                                    ),
                                    Ok(_) => {}
                                    Err(e) => tracing::warn!(
                                        component = "decision_log",
                                        error = %e,
                                        "prune failed"
                                    ),
                                }
                            }
                        }
                    }
                })
                .ok();
            tracing::info!(component = "decision_log", "decision journal initialised");
        }
        Err(e) => {
            tracing::error!(component = "decision_log", error = %e, "init failed — decisions will not be persisted");
        }
    }
}

/// Global accessor. `None` before `init` (e.g. in unit tests of other
/// modules) — call sites use `if let` so logging stays optional.
pub fn global() -> Option<&'static DecisionLog> {
    GLOBAL.get()
}

/// Fire-and-forget convenience: log into the global journal if it is
/// initialised, no-op otherwise.
pub fn log(record: DecisionRecord) {
    if let Some(l) = GLOBAL.get() {
        l.log(record);
    }
}

// ════════════════════════════════════════════════════════════════════
//  Tests
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_log() -> (DecisionLog, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "sentinel-declog-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("decisions.sqlite");
        (DecisionLog::new(path.clone(), true).expect("open"), dir)
    }

    #[test]
    fn log_and_query_roundtrip() {
        let (log, dir) = tmp_log();
        log.log(DecisionRecord::vetoed(
            "aapl",
            "5m",
            "gap_fade",
            "short",
            62.0,
            "spread",
            "spread too wide",
            1_700_000_000,
            187.5,
            "calm",
        ));
        log.log(DecisionRecord::entered(
            "AAPL",
            "5m",
            "prime",
            "long",
            75.0,
            Some(0.71),
            1_700_000_060,
            188.0,
            "trending",
        ));
        let now = chrono::Utc::now().timestamp();
        let rows = log.query("AAPL", 0, now + 10, 100).expect("query");
        assert_eq!(rows.len(), 2);
        // Newest first
        assert_eq!(rows[0].outcome, "entered");
        assert!((rows[0].conviction.unwrap() - 0.71).abs() < 1e-9);
        assert_eq!(rows[1].outcome, "vetoed");
        assert_eq!(rows[1].symbol, "AAPL", "symbol uppercased at boundary");
        assert_eq!(rows[1].veto_stage.as_deref(), Some("spread"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lifecycle_constructors_distinguish_queue_fill_and_execution_failure() {
        let dispatched = DecisionRecord::dispatched(
            "apex",
            "1m",
            "apex",
            "long",
            75.0,
            Some(0.62),
            100,
            10.0,
            "calm",
        );
        assert_eq!(dispatched.outcome, "dispatched");
        assert!(dispatched.veto_stage.is_none());

        let failed = DecisionRecord::execution_failed(
            "apex",
            "1m",
            "apex",
            "long",
            75.0,
            Some(0.62),
            100,
            10.0,
            "calm",
            "IOC ended canceled",
        );
        assert_eq!(failed.outcome, "execution_failed");
        assert_eq!(failed.veto_stage.as_deref(), Some("execution"));
        assert_eq!(failed.veto_reason.as_deref(), Some("IOC ended canceled"));
    }

    #[test]
    fn query_empty_symbol_returns_all() {
        let (log, dir) = tmp_log();
        for sym in ["AAA", "BBB"] {
            log.log(DecisionRecord::vetoed(
                sym,
                "1m",
                "fsm",
                "long",
                55.0,
                "quality_gate",
                "low efficiency",
                1_700_000_000,
                10.0,
                "calm",
            ));
        }
        let now = chrono::Utc::now().timestamp();
        let rows = log.query("", 0, now + 10, 100).expect("query");
        assert_eq!(rows.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn veto_summary_groups_and_sorts() {
        let (log, dir) = tmp_log();
        for _ in 0..3 {
            log.log(DecisionRecord::vetoed(
                "X",
                "1m",
                "fsm",
                "long",
                50.0,
                "quality_gate",
                "low efficiency",
                0,
                1.0,
                "calm",
            ));
        }
        log.log(DecisionRecord::vetoed(
            "X",
            "1m",
            "gap_fade",
            "short",
            60.0,
            "spread",
            "spread too wide",
            0,
            1.0,
            "calm",
        ));
        let summary = log.veto_summary(0).expect("summary");
        assert_eq!(summary.len(), 2);
        assert_eq!(summary[0].veto_reason, "low efficiency");
        assert_eq!(summary[0].count, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn buffer_cap_drops_oldest() {
        let dir = std::env::temp_dir().join(format!(
            "sentinel-declog-cap-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // sync_mode = false so the buffer accumulates without flushing.
        let log = DecisionLog::new(dir.join("d.sqlite"), false).expect("open");
        for i in 0..(BUFFER_CAP + 10) {
            log.log(DecisionRecord::vetoed(
                "X", "1m", "fsm", "long", i as f64, "s", "r", 0, 1.0, "calm",
            ));
        }
        {
            let buf = log.buffer.lock().unwrap();
            assert_eq!(buf.len(), BUFFER_CAP);
            // Oldest (raw_score 0..10) were dropped.
            assert!(buf[0].raw_score >= 10.0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_vetoed_filters_direction_and_price() {
        let (log, dir) = tmp_log();
        log.log(DecisionRecord::vetoed(
            "GOOD", "1m", "gap_fade", "short", 60.0, "spread", "wide", 100, 25.0, "calm",
        ));
        // direction "none" → excluded from shadow candidates
        log.log(DecisionRecord::vetoed(
            "SKIP1", "1m", "fsm", "none", 0.0, "universe", "sub-$2", 100, 1.5, "calm",
        ));
        // price 0 → unmarkable, excluded
        log.log(DecisionRecord::vetoed(
            "SKIP2",
            "1m",
            "fsm",
            "long",
            50.0,
            "quality_gate",
            "x",
            100,
            0.0,
            "calm",
        ));
        let rows = log.recent_vetoed(0, 100).expect("recent");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, "GOOD");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_is_idempotent_when_empty() {
        let (log, dir) = tmp_log();
        assert_eq!(log.flush().expect("flush"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
