//! D-PLUS (Sesiunea 2) — Market Memory DB.
//!
//! SQLite-backed memory of every closed trade plus its setup features.
//! The bot consults this DB at entry time:
//!
//!   1. Bucket the live setup's features (gap, rvol, vix, regime, etc.)
//!   2. Look up historical posterior for the same `(strategy, bucket_key)`
//!   3. If `n >= 30` AND `win_rate < 0.45` → veto the entry
//!   4. Otherwise log diagnostic and proceed
//!
//! ## Why SQLite, not a hand-rolled feature store?
//!
//! - **Durability**: WAL mode survives crashes; the alternative was a
//!   serde_json dump every N inserts which is fragile.
//! - **Indexability**: We need range queries over feature buckets;
//!   SQLite's B-tree index on `(strategy, regime, gap_bucket, rvol_bucket)`
//!   is < 1ms even at 100K rows.
//! - **No daemon**: rusqlite + bundled SQLite ships in-process; no
//!   external service to monitor or restart.
//!
//! ## Concurrency
//!
//! [`MemoryDb`] wraps a `Mutex<Connection>`. Every public method
//! locks for the duration of the call. Calls from the bot's hot path
//! are read-only (`posterior_expectancy`) and complete in <1ms even
//! under contention. Trade close writes (`record_close`) happen at
//! most a few times per minute — well below the SQLite serialised
//! write throughput ceiling.
//!
//! ## Cold-start behaviour
//!
//! The DB starts empty. `query_similar` returns an empty Vec, and
//! `posterior_expectancy` returns `None` (n < significance gate).
//! The bot's entry path treats `None` as "no veto" — entries proceed
//! as before. The DB only kicks in once it has accumulated enough
//! history (~30 trades per common bucket).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::smarttrend::strategies::{MarketContext, MarketRegime, StrategyId};

// ════════════════════════════════════════════════════════════════════
//  Schema constants
// ════════════════════════════════════════════════════════════════════

/// Bumped any time the schema changes; `MemoryDb::open` runs the
/// migration ladder up to this number.
///
/// History:
/// - v1 (S2): `setups`, `bayesian_posteriors`.
/// - v2 (S4 P0): `ml_samples` for OMEGA training (FeatureVec BLOB).
pub const SCHEMA_VERSION: i64 = 2;

/// Byte length of the FeatureVec BLOB stored in `ml_samples`.
/// Matches `crate::ml::features::FEATURE_DIM * size_of::<f64>() = 128 * 8`.
pub const ML_FEATURE_BLOB_BYTES: usize = 128 * 8;

/// Veto threshold — minimum historical samples before posterior
/// veto is allowed to fire. Below this, posterior returns `None`
/// and the bot's entry path treats it as "no signal".
pub const SIGNIFICANCE_N: i64 = 30;

/// Veto win-rate cutoff — posterior with `win_rate < this` AND
/// `n >= SIGNIFICANCE_N` blocks the entry.
pub const VETO_WIN_RATE: f64 = 0.45;

// ════════════════════════════════════════════════════════════════════
//  Public types
// ════════════════════════════════════════════════════════════════════

/// Snapshot of the features that define a setup. Built from a
/// [`MarketContext`] at entry time and persisted alongside the trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupFeatures {
    pub strategy: StrategyId,
    pub symbol: String,
    pub side: &'static str, // "long" | "short"
    pub gap_bucket: i64,
    pub rvol_bucket: i64,
    pub vix_bucket: i64,
    pub sector: String,
    pub time_of_day_bucket: i64,
    pub regime: String,
    pub atr_pct_bucket: i64,
    pub vol_z_bucket: i64,
    pub spread_bps_bucket: i64,
    pub news_recent: bool,
    pub earnings_today: bool,
}

impl SetupFeatures {
    /// Compose a deterministic feature key for use in
    /// `bayesian_posteriors.feature_key`. Order is stable so equal
    /// feature sets produce identical keys.
    pub fn key(&self) -> String {
        format!(
            "g{}_r{}_v{}_t{}_R{}_a{}_z{}_s{}_n{}_e{}_d{}",
            self.gap_bucket,
            self.rvol_bucket,
            self.vix_bucket,
            self.time_of_day_bucket,
            self.regime,
            self.atr_pct_bucket,
            self.vol_z_bucket,
            self.spread_bps_bucket,
            self.news_recent as i32,
            self.earnings_today as i32,
            self.side,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalSetup {
    pub id: i64,
    pub symbol: String,
    pub strategy: StrategyId,
    pub side: String,
    pub ts_open: i64,
    pub ts_close: Option<i64>,
    pub pnl: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Posterior {
    pub trades: i64,
    pub wins: i64,
    pub win_rate: f64,
    pub avg_pnl: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_setups: i64,
    pub closed_setups: i64,
    pub posteriors_trained: i64,
    pub last_write_ts: i64,
    pub schema_version: i64,
}

// ════════════════════════════════════════════════════════════════════
//  Bucketing — the cornerstone of "similar setup" lookups
// ════════════════════════════════════════════════════════════════════

/// Gap percentage → bucket. Symmetric coarse buckets so positive and
/// negative gaps don't collapse together.
pub fn bucket_gap(pct: f64) -> i64 {
    if pct.is_nan() {
        return 0;
    }
    // Infinities clamp to the extreme bucket of their sign — +inf → 5,
    // -inf → -5, mirroring the comparison ladder below.
    match pct {
        x if x <= -10.0 => -5,
        x if x <= -5.0 => -4,
        x if x <= -3.0 => -3,
        x if x <= -1.0 => -2,
        x if x < 1.0 => 0,
        x if x < 3.0 => 2,
        x if x < 5.0 => 3,
        x if x < 10.0 => 4,
        _ => 5,
    }
}

pub fn bucket_rvol(rvol: f64) -> i64 {
    if !rvol.is_finite() || rvol < 0.0 {
        return 0;
    }
    match rvol {
        x if x < 1.0 => 1,
        x if x < 2.0 => 2,
        x if x < 3.0 => 3,
        x if x < 5.0 => 4,
        _ => 5,
    }
}

pub fn bucket_vix(vix: f64) -> i64 {
    if !vix.is_finite() || vix < 0.0 {
        return 0;
    }
    match vix {
        x if x < 15.0 => 1,
        x if x < 20.0 => 2,
        x if x < 25.0 => 3,
        x if x < 30.0 => 4,
        _ => 5,
    }
}

/// 30-minute bucket within ET day. 0..47.
pub fn bucket_tod(now_et_min: u32) -> i64 {
    (now_et_min / 30).min(47) as i64
}

pub fn bucket_atr_pct(atr_pct: f64) -> i64 {
    if !atr_pct.is_finite() || atr_pct < 0.0 {
        return 0;
    }
    match atr_pct {
        x if x < 0.5 => 1,
        x if x < 1.0 => 2,
        x if x < 2.0 => 3,
        x if x < 5.0 => 4,
        _ => 5,
    }
}

pub fn bucket_vol_z(z: f64) -> i64 {
    if !z.is_finite() {
        return 0;
    }
    match z {
        x if x <= -3.0 => -3,
        x if x <= -2.0 => -2,
        x if x < -0.5 => -1,
        x if x < 0.5 => 0,
        x if x < 2.0 => 1,
        x if x < 3.0 => 2,
        _ => 3,
    }
}

pub fn bucket_spread_bps(bps: f64) -> i64 {
    if !bps.is_finite() || bps < 0.0 {
        return 0;
    }
    match bps {
        x if x < 5.0 => 1,
        x if x < 10.0 => 2,
        x if x < 25.0 => 3,
        x if x < 50.0 => 4,
        _ => 5,
    }
}

pub fn regime_label(regime: Option<MarketRegime>) -> &'static str {
    match regime {
        Some(MarketRegime::Calm) => "calm",
        Some(MarketRegime::Trending) => "trending",
        Some(MarketRegime::Volatile) => "volatile",
        Some(MarketRegime::Panic) => "panic",
        None => "unknown",
    }
}

/// Build a [`SetupFeatures`] from a live [`MarketContext`]. `vix` is
/// optional because the regime detector may not have populated it yet
/// at cold start.
pub fn features_from_ctx(
    ctx: &MarketContext,
    strategy: StrategyId,
    side: &'static str,
    vix: f64,
    sector: String,
    news_recent: bool,
    earnings_today: bool,
) -> SetupFeatures {
    let atr_pct = if ctx.price > 0.0 {
        ctx.atr / ctx.price * 100.0
    } else {
        0.0
    };
    SetupFeatures {
        strategy,
        symbol: ctx.symbol.clone(),
        side,
        gap_bucket: bucket_gap(ctx.gap_pct),
        rvol_bucket: bucket_rvol(ctx.rvol),
        vix_bucket: bucket_vix(vix),
        sector,
        time_of_day_bucket: bucket_tod(ctx.now_et_min),
        regime: regime_label(ctx.regime).to_string(),
        atr_pct_bucket: bucket_atr_pct(atr_pct),
        vol_z_bucket: 0, // Reserved — populated by causal engine if available
        spread_bps_bucket: bucket_spread_bps(ctx.spread_bps),
        news_recent,
        earnings_today,
    }
}

// ════════════════════════════════════════════════════════════════════
//  MemoryDb — public API
// ════════════════════════════════════════════════════════════════════

pub struct MemoryDb {
    conn: Mutex<Connection>,
    /// Path used for diagnostics; empty for in-memory DBs.
    pub path: PathBuf,
    /// In-memory cache of posteriors for lock-free read concurrency.
    pub posterior_cache: std::sync::RwLock<std::collections::HashMap<String, Posterior>>,
}

impl std::fmt::Debug for MemoryDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl MemoryDb {
    /// Open (or create) the DB at `path`. Runs migrations up to
    /// [`SCHEMA_VERSION`]. Use `MemoryDb::open_in_memory()` for tests.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::wrap_and_migrate(conn, path.to_path_buf())
    }

    /// In-memory DB for tests.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::wrap_and_migrate(conn, PathBuf::new())
    }

    fn wrap_and_migrate(conn: Connection, path: PathBuf) -> rusqlite::Result<Self> {
        // WAL improves write concurrency; ignore failure (in-memory DB
        // doesn't support WAL but the journal_mode pragma is harmless).
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        Self::run_migrations(&conn)?;

        let mut cache = std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT strategy, feature_key, trades, wins, sum_pnl FROM bayesian_posteriors"
            )?;
            let rows = stmt.query_map([], |r| {
                let strategy: String = r.get(0)?;
                let feature_key: String = r.get(1)?;
                let trades: i64 = r.get(2)?;
                let wins: i64 = r.get(3)?;
                let sum_pnl: f64 = r.get(4)?;
                Ok((strategy, feature_key, trades, wins, sum_pnl))
            })?;
            for row in rows {
                let (strategy, feature_key, trades, wins, sum_pnl) = row?;
                let win_rate = if trades > 0 { wins as f64 / trades as f64 } else { 0.0 };
                let avg_pnl = if trades > 0 { sum_pnl / trades as f64 } else { 0.0 };
                let key = format!("{}:{}", strategy, feature_key);
                cache.insert(key, Posterior {
                    trades,
                    wins,
                    win_rate,
                    avg_pnl,
                });
            }
        }

        Ok(Self {
            conn: Mutex::new(conn),
            path,
            posterior_cache: std::sync::RwLock::new(cache),
        })
    }

    fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (v INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS setups (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                symbol TEXT NOT NULL,
                strategy TEXT NOT NULL,
                side TEXT NOT NULL,
                ts_open INTEGER NOT NULL,
                ts_close INTEGER,
                pnl REAL,
                gap_bucket INTEGER NOT NULL,
                rvol_bucket INTEGER NOT NULL,
                vix_bucket INTEGER NOT NULL,
                sector TEXT NOT NULL,
                time_of_day_bucket INTEGER NOT NULL,
                regime TEXT NOT NULL,
                atr_pct_bucket INTEGER NOT NULL,
                vol_z_bucket INTEGER NOT NULL,
                spread_bps_bucket INTEGER NOT NULL,
                news_recent INTEGER NOT NULL,
                earnings_today INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_setup_lookup
                ON setups(strategy, regime, gap_bucket, rvol_bucket);
             CREATE INDEX IF NOT EXISTS idx_setup_open_ts
                ON setups(ts_open DESC);
             CREATE TABLE IF NOT EXISTS bayesian_posteriors (
                strategy TEXT NOT NULL,
                feature_key TEXT NOT NULL,
                trades INTEGER NOT NULL DEFAULT 0,
                wins INTEGER NOT NULL DEFAULT 0,
                sum_pnl REAL NOT NULL DEFAULT 0.0,
                last_update_ts INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (strategy, feature_key)
             );
             CREATE TABLE IF NOT EXISTS ml_samples (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                setup_id INTEGER,
                symbol TEXT NOT NULL,
                ts_open INTEGER NOT NULL,
                ts_close INTEGER NOT NULL,
                return_norm REAL NOT NULL,
                regime TEXT NOT NULL,
                feature_vec BLOB NOT NULL,
                source TEXT NOT NULL DEFAULT 'live'
             );
             CREATE INDEX IF NOT EXISTS idx_ml_samples_ts_close
                ON ml_samples(ts_close DESC);
             CREATE INDEX IF NOT EXISTS idx_ml_samples_symbol_ts
                ON ml_samples(symbol, ts_open);
             CREATE INDEX IF NOT EXISTS idx_ml_samples_regime_ts
                ON ml_samples(regime, ts_open);",
        )?;
        // Schema-version ladder. We only insert the marker row when the
        // current max version is below SCHEMA_VERSION. The IF NOT EXISTS
        // batch above is idempotent across reopens, so the only real
        // job here is to record that this DB has been migrated up.
        let cur: i64 = conn
            .query_row("SELECT COALESCE(MAX(v), 0) FROM schema_version", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        if cur < SCHEMA_VERSION {
            conn.execute(
                "INSERT INTO schema_version (v) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
        }
        Ok(())
    }

    /// Record a new setup at entry time. Returns the row id (used as
    /// `setup_id` to close the trade later).
    pub fn record_open(&self, features: &SetupFeatures, ts_open: i64) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO setups (
                symbol, strategy, side, ts_open,
                gap_bucket, rvol_bucket, vix_bucket, sector,
                time_of_day_bucket, regime, atr_pct_bucket,
                vol_z_bucket, spread_bps_bucket, news_recent, earnings_today
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                features.symbol,
                features.strategy.as_str(),
                features.side,
                ts_open,
                features.gap_bucket,
                features.rvol_bucket,
                features.vix_bucket,
                features.sector,
                features.time_of_day_bucket,
                features.regime,
                features.atr_pct_bucket,
                features.vol_z_bucket,
                features.spread_bps_bucket,
                features.news_recent as i64,
                features.earnings_today as i64,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Record the close of a previously-opened setup. Updates the
    /// `setups` row AND increments the matching `bayesian_posteriors`
    /// aggregate. No-op if `setup_id` is unknown (returns 0).
    pub fn record_close(&self, setup_id: i64, pnl: f64, ts_close: i64) -> rusqlite::Result<usize> {
        if !pnl.is_finite() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        // Update the setup row + fetch its features for posterior aggregation.
        let updated = conn.execute(
            "UPDATE setups SET pnl = ?1, ts_close = ?2 WHERE id = ?3 AND ts_close IS NULL",
            params![pnl, ts_close, setup_id],
        )?;
        if updated == 0 {
            return Ok(0);
        }
        // Fetch features for posterior key. Local struct keeps the
        // column order obvious and silences clippy::type_complexity
        // on a 13-element tuple.
        struct ClosedRow {
            strategy: String,
            gap: i64,
            rvol: i64,
            vix: i64,
            tod: i64,
            regime: String,
            atr: i64,
            vol_z: i64,
            spread: i64,
            news: i64,
            earnings: i64,
            side: String,
        }
        let row: Option<ClosedRow> = conn
            .query_row(
                "SELECT strategy, gap_bucket, rvol_bucket, vix_bucket, time_of_day_bucket,
                        regime, atr_pct_bucket, vol_z_bucket, spread_bps_bucket, news_recent,
                        earnings_today, side
                 FROM setups WHERE id = ?1",
                params![setup_id],
                |r| {
                    Ok(ClosedRow {
                        strategy: r.get(0)?,
                        gap: r.get(1)?,
                        rvol: r.get(2)?,
                        vix: r.get(3)?,
                        tod: r.get(4)?,
                        regime: r.get(5)?,
                        atr: r.get(6)?,
                        vol_z: r.get(7)?,
                        spread: r.get(8)?,
                        news: r.get(9)?,
                        earnings: r.get(10)?,
                        side: r.get(11)?,
                    })
                },
            )
            .optional()?;
        let Some(cr) = row else {
            return Ok(updated);
        };
        let key = format!(
            "g{}_r{}_v{}_t{}_R{}_a{}_z{}_s{}_n{}_e{}_d{}",
            cr.gap, cr.rvol, cr.vix, cr.tod, cr.regime, cr.atr, cr.vol_z, cr.spread, cr.news, cr.earnings, cr.side,
        );
        let strategy = cr.strategy;
        let win: i64 = if pnl > 0.0 { 1 } else { 0 };
        conn.execute(
            "INSERT INTO bayesian_posteriors (strategy, feature_key, trades, wins, sum_pnl, last_update_ts)
             VALUES (?1, ?2, 1, ?3, ?4, ?5)
             ON CONFLICT (strategy, feature_key) DO UPDATE SET
                trades = trades + 1,
                wins = wins + ?3,
                sum_pnl = sum_pnl + ?4,
                last_update_ts = ?5",
            params![strategy, key, win, pnl, ts_close],
        )?;

        // Update in-memory posterior cache
        let posterior_row: Option<(i64, i64, f64)> = conn
            .query_row(
                "SELECT trades, wins, sum_pnl FROM bayesian_posteriors
                 WHERE strategy = ?1 AND feature_key = ?2",
                params![&strategy, &key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((trades, wins, sum_pnl)) = posterior_row {
            let win_rate = wins as f64 / trades as f64;
            let avg_pnl = sum_pnl / trades as f64;
            let cache_key = format!("{}:{}", strategy, key);
            let mut cache = self.posterior_cache.write().unwrap();
            cache.insert(cache_key, Posterior {
                trades,
                wins,
                win_rate,
                avg_pnl,
            });
        }

        Ok(updated)
    }

    /// Look up similar historical setups (same `strategy` + bucket
    /// match on regime + gap + rvol). Returns `k` most-recent.
    pub fn query_similar(
        &self,
        features: &SetupFeatures,
        k: usize,
    ) -> rusqlite::Result<Vec<HistoricalSetup>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, symbol, strategy, side, ts_open, ts_close, pnl
               FROM setups
              WHERE strategy = ?1
                AND regime = ?2
                AND gap_bucket = ?3
                AND rvol_bucket = ?4
                AND ts_close IS NOT NULL
              ORDER BY ts_open DESC
              LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            params![
                features.strategy.as_str(),
                features.regime,
                features.gap_bucket,
                features.rvol_bucket,
                k as i64,
            ],
            |r| {
                let strat_str: String = r.get(2)?;
                let strategy = match strat_str.as_str() {
                    "gap_fade" => StrategyId::GapFade,
                    "etf_pair" => StrategyId::EtfPair,
                    "afternoon_fade" => StrategyId::AfternoonFade,
                    "stat_arb_50" => StrategyId::StatArb50,
                    "leveraged_etf" => StrategyId::LeveragedEtf,
                    "catalyst_frontrun" => StrategyId::CatalystFrontRun,
                    "halt_resume" => StrategyId::HaltResume,
                    "extended_news" => StrategyId::ExtendedNews,
                    _ => StrategyId::GapFade,
                };
                Ok(HistoricalSetup {
                    id: r.get(0)?,
                    symbol: r.get(1)?,
                    strategy,
                    side: r.get(3)?,
                    ts_open: r.get(4)?,
                    ts_close: r.get(5)?,
                    pnl: r.get(6)?,
                })
            },
        )?;
        rows.collect()
    }

    /// Returns the [`Posterior`] for `(strategy, features.key())` if
    /// `trades >= SIGNIFICANCE_N`. Otherwise `None`.
    pub fn posterior_expectancy(
        &self,
        features: &SetupFeatures,
    ) -> rusqlite::Result<Option<Posterior>> {
        let key = format!("{}:{}", features.strategy.as_str(), features.key());
        let cache = self.posterior_cache.read().unwrap();
        if let Some(post) = cache.get(&key) {
            if post.trades >= SIGNIFICANCE_N {
                return Ok(Some(post.clone()));
            }
        }
        Ok(None)
    }

    /// Returns the entry-veto verdict for the supplied features.
    /// `true` = veto (block entry), `false` = permit.
    pub fn should_veto_entry(&self, features: &SetupFeatures) -> rusqlite::Result<bool> {
        let p = self.posterior_expectancy(features)?;
        Ok(matches!(p, Some(post) if post.win_rate < VETO_WIN_RATE))
    }

    pub fn stats(&self) -> rusqlite::Result<MemoryStats> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let total_setups: i64 = conn
            .query_row("SELECT COUNT(*) FROM setups", [], |r| r.get(0))
            .unwrap_or(0);
        let closed_setups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM setups WHERE ts_close IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let posteriors_trained: i64 = conn
            .query_row("SELECT COUNT(*) FROM bayesian_posteriors", [], |r| r.get(0))
            .unwrap_or(0);
        let last_write_ts: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(last_update_ts), 0) FROM bayesian_posteriors",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let schema_version: i64 = conn
            .query_row("SELECT COALESCE(MAX(v), 0) FROM schema_version", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        Ok(MemoryStats {
            total_setups,
            closed_setups,
            posteriors_trained,
            last_write_ts,
            schema_version,
        })
    }

    // ════════════════════════════════════════════════════════════════
    //  ML samples API (S4 P0 — feeds OMEGA training)
    // ════════════════════════════════════════════════════════════════

    /// Persist a training sample for the OMEGA predictors.
    ///
    /// `feature_vec` MUST contain exactly `FEATURE_DIM = 128` f64
    /// elements; mismatched lengths return `Err` so callers cannot
    /// silently corrupt the table.
    ///
    /// `return_norm` is the normalised forward return used as the
    /// LSTM regression label (typically realised pnl divided by ATR
    /// at entry, clamped to a sane range).
    ///
    /// `regime` is the [`MarketRegime`] at sample time, used as the
    /// Transformer classification label.
    ///
    /// `source` is a free-form tag (`"live"`, `"bootstrap"`,
    /// `"replay"`) so we can later rebalance training sets.
    #[allow(clippy::too_many_arguments)]
    pub fn record_ml_sample(
        &self,
        feature_vec: &[f64],
        return_norm: f64,
        regime: MarketRegime,
        ts_open: i64,
        ts_close: i64,
        symbol: &str,
        setup_id: Option<i64>,
        source: &str,
    ) -> rusqlite::Result<i64> {
        if feature_vec.len() != ML_FEATURE_BLOB_BYTES / 8 {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "feature_vec len {} != expected {}",
                feature_vec.len(),
                ML_FEATURE_BLOB_BYTES / 8
            )));
        }
        if !return_norm.is_finite() {
            return Err(rusqlite::Error::InvalidParameterName(
                "return_norm must be finite".into(),
            ));
        }
        let mut blob = Vec::with_capacity(ML_FEATURE_BLOB_BYTES);
        for v in feature_vec {
            // NaN / Inf scrub at the boundary so training sees clean rows.
            let safe = if v.is_finite() { *v } else { 0.0 };
            blob.extend_from_slice(&safe.to_le_bytes());
        }
        let regime_label = regime_label(Some(regime));
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO ml_samples (
                setup_id, symbol, ts_open, ts_close,
                return_norm, regime, feature_vec, source
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                setup_id,
                symbol,
                ts_open,
                ts_close,
                return_norm,
                regime_label,
                blob,
                source,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Update the return_norm and ts_close of a previously recorded live ML sample.
    pub fn update_ml_sample_return(&self, id: i64, return_norm: f64, ts_close: i64) -> rusqlite::Result<()> {
        if !return_norm.is_finite() {
            return Err(rusqlite::Error::InvalidParameterName(
                "return_norm must be finite".into(),
            ));
        }
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "UPDATE ml_samples SET return_norm = ?1, ts_close = ?2 WHERE id = ?3",
            params![return_norm, ts_close, id],
        )?;
        Ok(())
    }

    /// Number of ML training samples currently stored.
    pub fn count_ml_samples(&self) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM ml_samples", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(n)
    }

    /// Returns up to `limit` most-recent `(feature_vec, return_norm)`
    /// pairs for LSTM regression training. Cold-start: empty vec.
    ///
    /// Order: most-recent first by `ts_close`. Caller is responsible
    /// for shuffling before training (we keep a stable order so tests
    /// can assert deterministically).
    pub fn pull_lstm_samples(
        &self,
        limit: usize,
    ) -> rusqlite::Result<Vec<(Vec<f64>, f64)>> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = conn.prepare(
            "SELECT feature_vec, return_norm
               FROM ml_samples
              ORDER BY ts_close DESC
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let blob: Vec<u8> = r.get(0)?;
            let ret: f64 = r.get(1)?;
            Ok((blob, ret))
        })?;
        let mut out: Vec<(Vec<f64>, f64)> = Vec::new();
        for row in rows {
            let (blob, ret) = row?;
            if blob.len() != ML_FEATURE_BLOB_BYTES {
                continue; // skip corrupt rows defensively
            }
            let mut fv = Vec::with_capacity(blob.len() / 8);
            for chunk in blob.chunks_exact(8) {
                let arr: [u8; 8] = match chunk.try_into() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                fv.push(f64::from_le_bytes(arr));
            }
            if fv.len() == ML_FEATURE_BLOB_BYTES / 8 && ret.is_finite() {
                out.push((fv, ret));
            }
        }
        Ok(out)
    }

    /// Returns up to `limit` `(sequence, regime)` tuples for the
    /// regime Transformer. Each `sequence` is a `Vec<Vec<f64>>` of
    /// exactly `seq_len` consecutive feature vectors from the same
    /// symbol, ordered oldest-first. The label is the regime tag of
    /// the last sample in the window.
    ///
    /// Cold-start (`<seq_len` rows per symbol) returns an empty vec.
    pub fn pull_xfmr_samples(
        &self,
        seq_len: usize,
        limit: usize,
    ) -> rusqlite::Result<Vec<(Vec<Vec<f64>>, MarketRegime)>> {
        if seq_len < 2 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        // Pull every row ordered by symbol then ts_open; we'll slice
        // sequences in Rust because SQL window functions add
        // complexity we don't need at this scale.
        let mut stmt = conn.prepare(
            "SELECT symbol, feature_vec, regime
               FROM ml_samples
              ORDER BY symbol ASC, ts_open ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            let sym: String = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let regime: String = r.get(2)?;
            Ok((sym, blob, regime))
        })?;
        let mut current_symbol = String::new();
        let mut window: Vec<Vec<f64>> = Vec::with_capacity(seq_len);
        let mut out: Vec<(Vec<Vec<f64>>, MarketRegime)> = Vec::new();
        for row in rows {
            if out.len() >= limit {
                break;
            }
            let (sym, blob, regime) = row?;
            if sym != current_symbol {
                window.clear();
                current_symbol = sym;
            }
            if blob.len() != ML_FEATURE_BLOB_BYTES {
                continue;
            }
            let mut fv = Vec::with_capacity(blob.len() / 8);
            for chunk in blob.chunks_exact(8) {
                let arr: [u8; 8] = match chunk.try_into() {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                fv.push(f64::from_le_bytes(arr));
            }
            if fv.len() != ML_FEATURE_BLOB_BYTES / 8 {
                continue;
            }
            window.push(fv);
            if window.len() > seq_len {
                window.remove(0);
            }
            if window.len() == seq_len {
                if let Some(r) = parse_regime_label(&regime) {
                    out.push((window.clone(), r));
                }
            }
        }
        Ok(out)
    }
}

/// Parse the `regime` text column back into a [`MarketRegime`]. Returns
/// `None` for `"unknown"` or anything else we don't recognise.
pub fn parse_regime_label(s: &str) -> Option<MarketRegime> {
    match s {
        "calm" => Some(MarketRegime::Calm),
        "trending" => Some(MarketRegime::Trending),
        "volatile" => Some(MarketRegime::Volatile),
        "panic" => Some(MarketRegime::Panic),
        _ => None,
    }
}

/// Default DB path: `~/.local/share/sentinel-trend/memory/memory.sqlite`.
pub fn default_db_path() -> PathBuf {
    let mut p = if let Some(h) = std::env::var_os("HOME") {
        PathBuf::from(h)
    } else {
        PathBuf::from(".")
    };
    p.push(".local/share/sentinel-trend/memory/memory.sqlite");
    p
}

// ════════════════════════════════════════════════════════════════════
//  Tests — 25 covering schema, buckets, persistence, posterior gates.
// ════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_features(strategy: StrategyId, side: &'static str) -> SetupFeatures {
        SetupFeatures {
            strategy,
            symbol: "TEST".to_string(),
            side,
            gap_bucket: 3,
            rvol_bucket: 2,
            vix_bucket: 1,
            sector: "tech".to_string(),
            time_of_day_bucket: 20,
            regime: "calm".to_string(),
            atr_pct_bucket: 2,
            vol_z_bucket: 0,
            spread_bps_bucket: 1,
            news_recent: false,
            earnings_today: false,
        }
    }

    #[test]
    fn open_in_memory_succeeds() {
        let db = MemoryDb::open_in_memory().expect("open");
        let stats = db.stats().expect("stats");
        assert_eq!(stats.total_setups, 0);
        assert_eq!(stats.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn record_open_returns_increasing_ids() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        let id1 = db.record_open(&f, 1000).expect("open1");
        let id2 = db.record_open(&f, 1001).expect("open2");
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn record_close_updates_setup_and_posterior() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        let id = db.record_open(&f, 1000).expect("open");
        let updated = db.record_close(id, 25.0, 1100).expect("close");
        assert_eq!(updated, 1);
        let stats = db.stats().expect("stats");
        assert_eq!(stats.closed_setups, 1);
        assert_eq!(stats.posteriors_trained, 1);
    }

    #[test]
    fn record_close_idempotent_on_already_closed() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        let id = db.record_open(&f, 1000).expect("open");
        let first = db.record_close(id, 25.0, 1100).expect("close1");
        let second = db.record_close(id, 30.0, 1200).expect("close2");
        assert_eq!(first, 1);
        assert_eq!(second, 0); // No second close
        // Posterior count unchanged
        let stats = db.stats().expect("stats");
        assert_eq!(stats.posteriors_trained, 1);
    }

    #[test]
    fn nan_pnl_close_is_noop() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        let id = db.record_open(&f, 1000).expect("open");
        let updated = db.record_close(id, f64::NAN, 1100).expect("close");
        assert_eq!(updated, 0);
        let stats = db.stats().expect("stats");
        assert_eq!(stats.closed_setups, 0);
    }

    #[test]
    fn unknown_setup_id_close_is_noop() {
        let db = MemoryDb::open_in_memory().expect("open");
        let updated = db.record_close(999, 25.0, 1100).expect("close");
        assert_eq!(updated, 0);
    }

    #[test]
    fn posterior_below_significance_returns_none() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        for i in 0..10 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            db.record_close(id, -10.0, 1100 + i).expect("close");
        }
        let p = db.posterior_expectancy(&f).expect("post");
        assert!(p.is_none()); // 10 < SIGNIFICANCE_N
    }

    #[test]
    fn posterior_at_significance_returns_some_with_correct_stats() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        for i in 0..30 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            // 18 wins of 50, 12 losses of -25 → win_rate 0.6, sum 600
            let pnl = if i < 18 { 50.0 } else { -25.0 };
            db.record_close(id, pnl, 1100 + i).expect("close");
        }
        let p = db.posterior_expectancy(&f).expect("post").unwrap();
        assert_eq!(p.trades, 30);
        assert_eq!(p.wins, 18);
        assert!((p.win_rate - 0.6).abs() < 1e-9);
        assert!((p.avg_pnl - (600.0_f64 / 30.0)).abs() < 1e-9);
    }

    #[test]
    fn veto_triggers_below_45_pct_with_significance() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        // 10 wins / 30 = 0.33 win_rate
        for i in 0..30 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            let pnl = if i < 10 { 5.0 } else { -10.0 };
            db.record_close(id, pnl, 1100 + i).expect("close");
        }
        assert!(db.should_veto_entry(&f).expect("veto"));
    }

    #[test]
    fn veto_does_not_trigger_when_winning() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        for i in 0..30 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            let pnl = if i < 20 { 5.0 } else { -10.0 };
            db.record_close(id, pnl, 1100 + i).expect("close");
        }
        assert!(!db.should_veto_entry(&f).expect("veto"));
    }

    #[test]
    fn query_similar_returns_only_matching_strategy_and_buckets() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f1 = sample_features(StrategyId::GapFade, "long");
        let mut f2 = sample_features(StrategyId::EtfPair, "long");
        f2.gap_bucket = -3;
        for i in 0..3 {
            let id = db.record_open(&f1, 1000 + i).expect("open");
            db.record_close(id, 5.0, 1100 + i).expect("close");
        }
        let id = db.record_open(&f2, 1500).expect("open");
        db.record_close(id, 5.0, 1600).expect("close");
        let hits = db.query_similar(&f1, 10).expect("query");
        assert_eq!(hits.len(), 3);
        for h in &hits {
            assert_eq!(h.strategy, StrategyId::GapFade);
        }
    }

    #[test]
    fn query_similar_returns_empty_on_cold_start() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        let hits = db.query_similar(&f, 10).expect("query");
        assert!(hits.is_empty());
    }

    #[test]
    fn bucket_gap_handles_extremes() {
        assert_eq!(bucket_gap(-15.0), -5);
        assert_eq!(bucket_gap(-7.5), -4);
        assert_eq!(bucket_gap(0.0), 0);
        assert_eq!(bucket_gap(7.5), 4);
        assert_eq!(bucket_gap(15.0), 5);
        assert_eq!(bucket_gap(f64::NAN), 0);
        assert_eq!(bucket_gap(f64::INFINITY), 5);
    }

    #[test]
    fn bucket_rvol_clamps_negative() {
        assert_eq!(bucket_rvol(-1.0), 0);
        assert_eq!(bucket_rvol(0.5), 1);
        assert_eq!(bucket_rvol(1.5), 2);
        assert_eq!(bucket_rvol(10.0), 5);
    }

    #[test]
    fn bucket_vix_handles_panic_levels() {
        assert_eq!(bucket_vix(12.0), 1);
        assert_eq!(bucket_vix(45.0), 5); // 2008 / Mar 2020 panic
    }

    #[test]
    fn bucket_tod_clamps_to_47() {
        assert_eq!(bucket_tod(0), 0);
        assert_eq!(bucket_tod(30), 1);
        assert_eq!(bucket_tod(1439), 47);
        assert_eq!(bucket_tod(99999), 47);
    }

    #[test]
    fn feature_key_is_deterministic() {
        let f1 = sample_features(StrategyId::GapFade, "long");
        let f2 = sample_features(StrategyId::GapFade, "long");
        assert_eq!(f1.key(), f2.key());
    }

    #[test]
    fn feature_key_changes_with_any_field() {
        let f1 = sample_features(StrategyId::GapFade, "long");
        let mut f2 = f1.clone();
        f2.gap_bucket += 1;
        assert_ne!(f1.key(), f2.key());
    }

    #[test]
    fn schema_version_recorded_on_open() {
        let db = MemoryDb::open_in_memory().expect("open");
        let s = db.stats().expect("stats");
        assert_eq!(s.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn migrations_idempotent_on_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "sentinel-memdb-mig-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.sqlite");
        let db1 = MemoryDb::open(&path).expect("open1");
        drop(db1);
        let db2 = MemoryDb::open(&path).expect("open2");
        assert_eq!(db2.stats().unwrap().schema_version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_db_survives_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "sentinel-memdb-survive-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.sqlite");
        {
            let db = MemoryDb::open(&path).expect("open1");
            let f = sample_features(StrategyId::GapFade, "long");
            let id = db.record_open(&f, 1000).expect("open");
            db.record_close(id, 25.0, 1100).expect("close");
        }
        {
            let db = MemoryDb::open(&path).expect("open2");
            assert_eq!(db.stats().unwrap().closed_setups, 1);
            assert_eq!(db.stats().unwrap().posteriors_trained, 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn posterior_aggregates_across_multiple_setups_same_key() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        for i in 0..30 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            db.record_close(id, if i % 2 == 0 { 10.0 } else { -5.0 }, 1100 + i)
                .expect("close");
        }
        let p = db.posterior_expectancy(&f).expect("post").unwrap();
        assert_eq!(p.trades, 30);
        assert_eq!(p.wins, 15);
    }

    #[test]
    fn posterior_separated_by_strategy() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f_gap = sample_features(StrategyId::GapFade, "long");
        let f_etf = sample_features(StrategyId::EtfPair, "long");
        for i in 0..30 {
            let id = db.record_open(&f_gap, 1000 + i).expect("open");
            db.record_close(id, 10.0, 1100 + i).expect("close");
        }
        for i in 0..30 {
            let id = db.record_open(&f_etf, 2000 + i).expect("open");
            db.record_close(id, -10.0, 2100 + i).expect("close");
        }
        let p_gap = db.posterior_expectancy(&f_gap).unwrap().unwrap();
        let p_etf = db.posterior_expectancy(&f_etf).unwrap().unwrap();
        assert_eq!(p_gap.wins, 30);
        assert_eq!(p_etf.wins, 0);
    }

    #[test]
    fn regime_label_covers_all_variants() {
        assert_eq!(regime_label(Some(MarketRegime::Calm)), "calm");
        assert_eq!(regime_label(Some(MarketRegime::Trending)), "trending");
        assert_eq!(regime_label(Some(MarketRegime::Volatile)), "volatile");
        assert_eq!(regime_label(Some(MarketRegime::Panic)), "panic");
        assert_eq!(regime_label(None), "unknown");
    }

    #[test]
    fn stats_count_is_consistent() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        for i in 0..5 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            if i < 3 {
                db.record_close(id, 5.0, 1100 + i).expect("close");
            }
        }
        let s = db.stats().expect("stats");
        assert_eq!(s.total_setups, 5);
        assert_eq!(s.closed_setups, 3);
    }

    #[test]
    fn default_db_path_resolves() {
        let p = default_db_path();
        assert!(p.ends_with("memory.sqlite"));
    }

    #[test]
    fn query_similar_respects_limit_k() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");
        for i in 0..50 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            db.record_close(id, 5.0, 1100 + i).expect("close");
        }
        let hits = db.query_similar(&f, 10).expect("query");
        assert_eq!(hits.len(), 10);
        // Most-recent ordering
        assert!(hits[0].ts_open > hits[9].ts_open);
    }

    // ════════════════════════════════════════════════════════════
    //  ml_samples — S4 P0 (OMEGA training feed)
    // ════════════════════════════════════════════════════════════

    fn sample_fv(seed: f64) -> Vec<f64> {
        let mut v = Vec::with_capacity(ML_FEATURE_BLOB_BYTES / 8);
        for i in 0..(ML_FEATURE_BLOB_BYTES / 8) {
            v.push(seed + i as f64 * 0.001);
        }
        v
    }

    #[test]
    fn schema_v2_records_correct_version() {
        let db = MemoryDb::open_in_memory().expect("open");
        let s = db.stats().expect("stats");
        assert_eq!(s.schema_version, 2);
        assert_eq!(SCHEMA_VERSION, 2);
    }

    #[test]
    fn record_ml_sample_happy_path_returns_id() {
        let db = MemoryDb::open_in_memory().expect("open");
        let fv = sample_fv(0.0);
        let id = db
            .record_ml_sample(
                &fv,
                0.123,
                MarketRegime::Calm,
                1000,
                1500,
                "AAPL",
                None,
                "bootstrap",
            )
            .expect("record");
        assert_eq!(id, 1);
        assert_eq!(db.count_ml_samples().expect("count"), 1);
    }

    #[test]
    fn record_ml_sample_rejects_wrong_length() {
        let db = MemoryDb::open_in_memory().expect("open");
        let fv = vec![0.0; 64]; // wrong length
        let res = db.record_ml_sample(
            &fv,
            0.0,
            MarketRegime::Calm,
            1000,
            1500,
            "AAPL",
            None,
            "test",
        );
        assert!(res.is_err());
        assert_eq!(db.count_ml_samples().expect("count"), 0);
    }

    #[test]
    fn record_ml_sample_rejects_non_finite_return() {
        let db = MemoryDb::open_in_memory().expect("open");
        let fv = sample_fv(0.0);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let res = db.record_ml_sample(
                &fv,
                bad,
                MarketRegime::Calm,
                1000,
                1500,
                "AAPL",
                None,
                "test",
            );
            assert!(res.is_err(), "should reject {bad}");
        }
        assert_eq!(db.count_ml_samples().expect("count"), 0);
    }

    #[test]
    fn record_ml_sample_scrubs_nan_inf_in_feature_vec() {
        let db = MemoryDb::open_in_memory().expect("open");
        let mut fv = sample_fv(1.0);
        fv[5] = f64::NAN;
        fv[10] = f64::INFINITY;
        fv[15] = f64::NEG_INFINITY;
        let id = db
            .record_ml_sample(
                &fv,
                0.0,
                MarketRegime::Calm,
                1000,
                1500,
                "X",
                None,
                "test",
            )
            .expect("record");
        assert_eq!(id, 1);
        let pulled = db.pull_lstm_samples(10).expect("pull");
        assert_eq!(pulled.len(), 1);
        assert!(pulled[0].0.iter().all(|x| x.is_finite()));
        assert_eq!(pulled[0].0[5], 0.0);
        assert_eq!(pulled[0].0[10], 0.0);
        assert_eq!(pulled[0].0[15], 0.0);
    }

    #[test]
    fn count_ml_samples_zero_on_cold_start() {
        let db = MemoryDb::open_in_memory().expect("open");
        assert_eq!(db.count_ml_samples().expect("count"), 0);
    }

    #[test]
    fn pull_lstm_samples_empty_on_cold_start() {
        let db = MemoryDb::open_in_memory().expect("open");
        let pulled = db.pull_lstm_samples(100).expect("pull");
        assert!(pulled.is_empty());
    }

    #[test]
    fn pull_lstm_samples_orders_most_recent_first() {
        let db = MemoryDb::open_in_memory().expect("open");
        for i in 0..5 {
            let fv = sample_fv(i as f64);
            db.record_ml_sample(
                &fv,
                i as f64 * 0.1,
                MarketRegime::Calm,
                1000 + i,
                1500 + i, // ts_close grows monotonically
                "AAPL",
                None,
                "test",
            )
            .expect("record");
        }
        let pulled = db.pull_lstm_samples(10).expect("pull");
        assert_eq!(pulled.len(), 5);
        // Newest first → first row's return_norm should be highest (0.4)
        assert!((pulled[0].1 - 0.4).abs() < 1e-9);
        assert!((pulled[4].1 - 0.0).abs() < 1e-9);
    }

    #[test]
    fn pull_lstm_samples_respects_limit() {
        let db = MemoryDb::open_in_memory().expect("open");
        for i in 0..20 {
            let fv = sample_fv(i as f64);
            db.record_ml_sample(
                &fv,
                0.1,
                MarketRegime::Calm,
                1000 + i,
                1500 + i,
                "AAPL",
                None,
                "test",
            )
            .expect("record");
        }
        let pulled = db.pull_lstm_samples(7).expect("pull");
        assert_eq!(pulled.len(), 7);
    }

    #[test]
    fn pull_lstm_samples_blob_round_trip_preserves_values() {
        let db = MemoryDb::open_in_memory().expect("open");
        let fv = sample_fv(42.0); // distinct values across 128 dims
        db.record_ml_sample(
            &fv,
            0.5,
            MarketRegime::Volatile,
            1000,
            1500,
            "X",
            None,
            "test",
        )
        .expect("record");
        let pulled = db.pull_lstm_samples(1).expect("pull");
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].0.len(), fv.len());
        for i in 0..fv.len() {
            assert!(
                (pulled[0].0[i] - fv[i]).abs() < 1e-12,
                "dim {i}: got {} expected {}",
                pulled[0].0[i],
                fv[i]
            );
        }
    }

    #[test]
    fn pull_xfmr_samples_empty_when_not_enough_per_symbol() {
        let db = MemoryDb::open_in_memory().expect("open");
        for i in 0..10 {
            let fv = sample_fv(i as f64);
            db.record_ml_sample(
                &fv,
                0.0,
                MarketRegime::Calm,
                1000 + i,
                1500 + i,
                "X",
                None,
                "test",
            )
            .expect("record");
        }
        // Need 60 frames per symbol; we have only 10.
        let pulled = db.pull_xfmr_samples(60, 100).expect("pull");
        assert!(pulled.is_empty());
    }

    #[test]
    fn pull_xfmr_samples_returns_full_windows() {
        let db = MemoryDb::open_in_memory().expect("open");
        for i in 0..5 {
            let fv = sample_fv(i as f64);
            db.record_ml_sample(
                &fv,
                0.0,
                MarketRegime::Trending,
                1000 + i,
                1500 + i,
                "AAPL",
                None,
                "test",
            )
            .expect("record");
        }
        // seq_len=3 → 3 sliding windows possible from 5 rows.
        let pulled = db.pull_xfmr_samples(3, 100).expect("pull");
        assert_eq!(pulled.len(), 3);
        for (seq, regime) in &pulled {
            assert_eq!(seq.len(), 3);
            assert_eq!(*regime, MarketRegime::Trending);
        }
    }

    #[test]
    fn pull_xfmr_samples_does_not_cross_symbols() {
        let db = MemoryDb::open_in_memory().expect("open");
        // 5 rows symbol AAPL, 5 rows symbol MSFT — each side too short
        // for seq_len=3? No: each can produce 3 windows. But windows
        // must NOT mix the two symbols.
        for sym in ["AAPL", "MSFT"] {
            for i in 0..5 {
                let fv = sample_fv(i as f64);
                db.record_ml_sample(
                    &fv,
                    0.0,
                    if sym == "AAPL" {
                        MarketRegime::Calm
                    } else {
                        MarketRegime::Panic
                    },
                    1000 + i,
                    1500 + i,
                    sym,
                    None,
                    "test",
                )
                .expect("record");
            }
        }
        let pulled = db.pull_xfmr_samples(3, 100).expect("pull");
        // 3 windows per symbol × 2 symbols = 6 expected.
        assert_eq!(pulled.len(), 6);
        // Group by regime and check counts (regime tags differ per symbol).
        let calm_count = pulled.iter().filter(|(_, r)| *r == MarketRegime::Calm).count();
        let panic_count = pulled.iter().filter(|(_, r)| *r == MarketRegime::Panic).count();
        assert_eq!(calm_count, 3);
        assert_eq!(panic_count, 3);
    }

    #[test]
    fn pull_xfmr_samples_seq_len_below_two_returns_empty() {
        let db = MemoryDb::open_in_memory().expect("open");
        let fv = sample_fv(0.0);
        for i in 0..10 {
            db.record_ml_sample(
                &fv,
                0.0,
                MarketRegime::Calm,
                1000 + i,
                1500 + i,
                "X",
                None,
                "test",
            )
            .expect("record");
        }
        let pulled = db.pull_xfmr_samples(1, 100).expect("pull");
        assert!(pulled.is_empty());
        let pulled = db.pull_xfmr_samples(0, 100).expect("pull");
        assert!(pulled.is_empty());
    }

    #[test]
    fn pull_xfmr_samples_respects_limit() {
        let db = MemoryDb::open_in_memory().expect("open");
        for i in 0..20 {
            let fv = sample_fv(i as f64);
            db.record_ml_sample(
                &fv,
                0.0,
                MarketRegime::Calm,
                1000 + i,
                1500 + i,
                "X",
                None,
                "test",
            )
            .expect("record");
        }
        let pulled = db.pull_xfmr_samples(3, 5).expect("pull");
        assert_eq!(pulled.len(), 5);
    }

    #[test]
    fn parse_regime_label_round_trips_all_variants() {
        for r in [
            MarketRegime::Calm,
            MarketRegime::Trending,
            MarketRegime::Volatile,
            MarketRegime::Panic,
        ] {
            let label = regime_label(Some(r));
            assert_eq!(parse_regime_label(label), Some(r));
        }
    }

    #[test]
    fn parse_regime_label_rejects_unknown() {
        assert!(parse_regime_label("unknown").is_none());
        assert!(parse_regime_label("").is_none());
        assert!(parse_regime_label("garbage").is_none());
    }

    #[test]
    fn ml_samples_persists_across_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "sentinel-memdb-ml-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.sqlite");
        {
            let db = MemoryDb::open(&path).expect("open1");
            let fv = sample_fv(7.0);
            db.record_ml_sample(
                &fv,
                0.25,
                MarketRegime::Trending,
                1000,
                1500,
                "AAPL",
                None,
                "bootstrap",
            )
            .expect("record");
        }
        {
            let db = MemoryDb::open(&path).expect("open2");
            assert_eq!(db.count_ml_samples().expect("count"), 1);
            let pulled = db.pull_lstm_samples(10).expect("pull");
            assert_eq!(pulled.len(), 1);
            assert!((pulled[0].1 - 0.25).abs() < 1e-12);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_ml_sample_blob_is_skipped_at_pull() {
        // Insert a valid sample, then directly inject a corrupt
        // blob via a raw SQL execute, then verify pull skips it.
        let db = MemoryDb::open_in_memory().expect("open");
        let fv = sample_fv(0.0);
        db.record_ml_sample(
            &fv,
            0.1,
            MarketRegime::Calm,
            1000,
            1500,
            "X",
            None,
            "test",
        )
        .expect("record");
        {
            let conn = db.conn.lock().unwrap_or_else(|p| p.into_inner());
            let bad_blob: Vec<u8> = vec![0u8; 100]; // wrong length
            conn.execute(
                "INSERT INTO ml_samples (
                    setup_id, symbol, ts_open, ts_close,
                    return_norm, regime, feature_vec, source
                 ) VALUES (NULL, 'Y', 2000, 2500, 0.0, 'calm', ?1, 'corrupt')",
                params![bad_blob],
            )
            .expect("inject");
        }
        let pulled = db.pull_lstm_samples(10).expect("pull");
        assert_eq!(pulled.len(), 1, "corrupt row should be skipped");
        assert_eq!(db.count_ml_samples().expect("count"), 2);
    }

    #[test]
    fn test_update_ml_sample_return() {
        let db = MemoryDb::open_in_memory().expect("open");
        let fv = sample_fv(0.0);
        let id = db.record_ml_sample(
            &fv,
            0.0,
            MarketRegime::Calm,
            1000,
            0,
            "AAPL",
            Some(42),
            "test",
        )
        .expect("record");
        
        let res = db.update_ml_sample_return(id, 1.25, 2000);
        assert!(res.is_ok());
        
        let pulled = db.pull_lstm_samples(10).expect("pull");
        assert_eq!(pulled.len(), 1);
        assert!((pulled[0].1 - 1.25).abs() < 1e-9);
    }

    #[test]
    fn test_posterior_cache_integration() {
        let db = MemoryDb::open_in_memory().expect("open");
        let f = sample_features(StrategyId::GapFade, "long");

        assert!(db.posterior_cache.read().unwrap().is_empty());

        for i in 0..30 {
            let id = db.record_open(&f, 1000 + i).expect("open");
            let pnl = if i < 18 { 50.0 } else { -25.0 };
            db.record_close(id, pnl, 1100 + i).expect("close");
        }

        let cache_key = format!("{}:{}", f.strategy.as_str(), f.key());
        {
            let cache = db.posterior_cache.read().unwrap();
            assert_eq!(cache.len(), 1);
            let post = cache.get(&cache_key).expect("cached item");
            assert_eq!(post.trades, 30);
            assert_eq!(post.wins, 18);
            assert!((post.win_rate - 0.6).abs() < 1e-9);
        }

        let post = db.posterior_expectancy(&f).expect("expectancy").expect("some");
        assert_eq!(post.trades, 30);
        assert_eq!(post.wins, 18);

        let dir = std::env::temp_dir().join(format!(
            "sentinel-memdb-cache-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.sqlite");
        {
            let db_persist = MemoryDb::open(&path).expect("open1");
            for i in 0..30 {
                let id = db_persist.record_open(&f, 1000 + i).expect("open");
                let pnl = if i < 18 { 50.0 } else { -25.0 };
                db_persist.record_close(id, pnl, 1100 + i).expect("close");
            }
        }
        {
            let db_reopened = MemoryDb::open(&path).expect("open2");
            let cache = db_reopened.posterior_cache.read().unwrap();
            assert_eq!(cache.len(), 1);
            let post = cache.get(&cache_key).expect("cached item");
            assert_eq!(post.trades, 30);
            assert_eq!(post.wins, 18);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
