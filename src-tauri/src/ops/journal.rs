use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{NaiveDate, TimeZone, Utc};
use serde::Serialize;

use crate::error::TitanResult;
use crate::paths;
use crate::state::{Direction, FsmPhase, JournalEntry, TitanState};

use super::persistence::{load_with_fallback, save_atomic, save_hash_atomic};

static LAST_ARCHIVE_DATE: Mutex<Option<NaiveDate>> = Mutex::new(None);
const ARCHIVE_RETENTION_DAYS: i64 = 30;

// ═══════════════════════════════════════════════════════════════════════
//  Journal file path
// ═══════════════════════════════════════════════════════════════════════

fn journal_path() -> PathBuf {
    paths::journal_dir().join("trades.json")
}

fn journal_hash_path() -> PathBuf {
    paths::journal_dir().join("trades.hash")
}

fn journal_archive_path(date: NaiveDate) -> PathBuf {
    paths::journal_dir().join(format!("trades-{}.json", date.format("%Y-%m-%d")))
}

fn maybe_archive_and_prune(entries: &[JournalEntry]) {
    let today_et = Utc::now()
        .with_timezone(&chrono_tz::US::Eastern)
        .date_naive();

    let mut last = LAST_ARCHIVE_DATE.lock().unwrap_or_else(|p| p.into_inner());
    if *last == Some(today_et) {
        return;
    }

    let archive_date = today_et
        .pred_opt()
        .unwrap_or(today_et);
    let archive_path = journal_archive_path(archive_date);
    if !archive_path.exists() && !entries.is_empty() {
        if let Err(e) = save_atomic(&archive_path, &entries) {
            tracing::warn!(
                component = "journal",
                path = %archive_path.display(),
                "Failed to create daily archive: {e}"
            );
        } else {
            tracing::info!(
                component = "journal",
                date = %archive_date,
                entries = entries.len(),
                "Daily journal archive created"
            );
        }
    }

    prune_old_archives(today_et);
    *last = Some(today_et);
}

fn prune_old_archives(today: NaiveDate) {
    let cutoff = today - chrono::Duration::days(ARCHIVE_RETENTION_DAYS);
    let dir = paths::journal_dir();
    let Ok(read_dir) = fs::read_dir(&dir) else { return };

    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if !name_str.starts_with("trades-") || !name_str.ends_with(".json") {
            continue;
        }
        let date_part = &name_str["trades-".len()..name_str.len() - ".json".len()];
        if let Ok(date) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
            if date < cutoff {
                if let Err(e) = fs::remove_file(entry.path()) {
                    tracing::warn!(
                        component = "journal",
                        path = %entry.path().display(),
                        "Failed to prune old archive: {e}"
                    );
                } else {
                    tracing::debug!(
                        component = "journal",
                        date = %date,
                        "Pruned old journal archive"
                    );
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  Record trade open / close
// ═══════════════════════════════════════════════════════════════════════

pub struct TradeOpenParams {
    pub trade_id: String,
    pub symbol: String,
    pub side: Direction,
    pub entry_price: f64,
    pub qty: f64,
    pub entry_time: i64,
    pub entry_score: f64,
    pub fsm_phase: FsmPhase,
    /// v2.5.0 NUCLEAR (Faza 5) — Tournament strategy that originated
    /// this trade (parsed from the canonical `s2:<strategy>` order
    /// `source` value). `None` for legacy FSM trades.
    pub strategy_id: Option<String>,
    /// Timeframe the entry fired on ("1m" / "5m" / …). Recovered from the
    /// authoring order; empty for legacy / broker-only positions.
    pub timeframe: String,
    /// PRIME F3 — fused conviction of the entry decision. `None` for
    /// legacy / manual / recon-recovered rows.
    pub conviction: Option<f64>,
    /// P2 — regime label at entry (context-conditional reliability).
    pub regime: Option<String>,
}

/// Creates a new JournalEntry when a position is opened.
/// Called from trade_updates when a new position is created from a fill.
/// Accepts score/phase via params to avoid locking smarttrend (lock ordering safe).
pub fn record_trade_open(state: &TitanState, params: TradeOpenParams) {
    let mut journal = state.journal.lock().unwrap();

    // S16.4: Dedup against an existing OPEN entry for the same symbol.
    // Without this guard, callers like `position_recon` (which fires every
    // 15s and runs at every restart) accumulate one phantom open entry
    // per restart for positions that never actually re-opened. Closes are
    // still applied via record_trade_close which matches by trade_id.
    let already_open = journal
        .entries
        .iter()
        .any(|e| e.symbol == params.symbol && e.exit_time.is_none());
    if already_open {
        tracing::debug!(
            component = "journal",
            symbol = %params.symbol,
            trade_id = %params.trade_id,
            "Trade open skipped — existing OPEN entry for this symbol"
        );
        return;
    }

    let entry = JournalEntry {
        trade_id: params.trade_id,
        symbol: params.symbol,
        side: params.side,
        entry_price: params.entry_price,
        exit_price: None,
        qty: params.qty,
        pnl: None,
        entry_time: params.entry_time,
        exit_time: None,
        exit_reason: None,
        entry_score: params.entry_score,
        fsm_phase_at_entry: params.fsm_phase,
        strategy_id: params.strategy_id,
        timeframe: params.timeframe,
        conviction: params.conviction,
        regime: params.regime,
    };

    journal.entries.push(entry);
    journal.dirty = true;

    tracing::info!(
        component = "journal",
        symbol = %journal.entries.last().unwrap().symbol,
        side = ?journal.entries.last().unwrap().side,
        "Trade opened — journal entry created"
    );
}

/// Closes an open JournalEntry when a position is fully closed.
/// S06: matches by trade_id first (deterministic).
/// S16.5: Falls back to symbol match whenever the trade_id lookup fails
/// (not only when trade_id is empty). This is required because the entry
/// can be opened by `position_recon` with a synthetic id (e.g.
/// `recon-LWLG-1777664148`) while `trade_updates` reports the close with
/// the actual broker order id of the SELL — those will never match
/// directly. Symbol match is safe because we also gate on `exit_time IS
/// NULL`, so only the still-open entry for that symbol can be claimed.
/// Returns `true` iff an OPEN entry was found and flipped to CLOSED in
/// this call. Callers that mutate side effects (e.g. wallet credit) MUST
/// gate on the return value to remain idempotent across the
/// `position_recon` ↔ `trade_updates` race.
pub fn record_trade_close(
    state: &TitanState,
    trade_id: &str,
    symbol: &str,
    exit_price: f64,
    pnl: f64,
    exit_reason: &str,
) -> bool {
    let mut journal = state.journal.lock().unwrap();

    // First try: deterministic match by trade_id (when both producer and
    // consumer agree on the id).
    let by_id = if !trade_id.is_empty() {
        journal
            .entries
            .iter()
            .rposition(|e| e.trade_id == trade_id && e.exit_time.is_none())
    } else {
        None
    };

    // Fallback: any open entry for this symbol (newest first via rev).
    let entry = match by_id {
        Some(idx) => journal.entries.get_mut(idx),
        None => journal
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.symbol == symbol && e.exit_time.is_none()),
    };

    if let Some(entry) = entry {
        entry.exit_price = Some(exit_price);
        entry.exit_time = Some(Utc::now().timestamp());
        entry.pnl = Some(pnl);
        entry.exit_reason = Some(exit_reason.to_string());
        let proceeds = entry.qty * exit_price;
        journal.dirty = true;

        tracing::info!(
            component = "journal",
            symbol = %symbol,
            pnl = format!("{pnl:+.2}"),
            reason = %exit_reason,
            "Trade closed — journal entry updated"
        );
        // Drop the journal lock before touching the bandit so we
        // never hold two state-tier locks at once.
        drop(journal);

        {
            let mut r = state.risk.lock().unwrap_or_else(|p| p.into_inner());
            let now = chrono::Utc::now().timestamp();
            r.add_pending_settlement(proceeds, now);
        }

        update_bandit_on_close(state, symbol, pnl);
        true
    } else {
        tracing::warn!(
            component = "journal",
            symbol = %symbol,
            "No open journal entry found for close"
        );
        false
    }
}

/// FRONTIER (Sesiunea 4) — feed the realised P&L back into the
/// contextual bandit so it can learn which `(regime, confidence)`
/// contexts deserve which size multiplier.
///
/// Idempotent: removes the pending entry on first call. A second
/// call for the same symbol (e.g., from a stale recon path) is a
/// no-op because the pending row is gone.
///
/// Failure semantics: any error inside the bandit is swallowed at
/// `warn` level — the journal write must never roll back because
/// the bandit had a bad day.
fn update_bandit_on_close(state: &TitanState, symbol: &str, pnl: f64) {
    let pending = {
        let mut map = state
            .bandit_pending
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        map.remove(symbol)
    };
    let Some(ctx) = pending else {
        // No pending row — bot picked the cold-start arm at entry,
        // or the trade was opened pre-S4. Nothing to learn here.
        return;
    };
    let profitable = pnl > 0.0;
    let mut bandit = state.bandit.lock().unwrap_or_else(|p| p.into_inner());
    bandit.update(ctx.regime, ctx.confidence, ctx.arm_idx, profitable);
    if let Err(e) = bandit.save(&crate::ml::bandit::default_state_path()) {
        tracing::warn!(
            component = "bandit",
            error = %e,
            "Failed to persist bandit state after update"
        );
    }
    tracing::info!(
        component = "bandit",
        symbol = %symbol,
        arm = ctx.arm_idx,
        profitable = profitable,
        total_updates = bandit.total_observations(),
        "Bandit updated from trade close"
    );
}

/// **S18** — combined post-close hook: feeds both the S17.1 intraday
/// loss-streak counter AND the AutoUniverse blacklist machinery from
/// a single realised close.
///
/// This is the canonical post-close hook. New code paths should call
/// this; legacy paths that only need the streak side may continue
/// calling `update_loss_streak` directly. Idempotent on duplicate
/// calls only insofar as the underlying primitives are.
///
/// `allocated_usd` is the dollars committed to the entry (`entry_price *
/// qty`). Pass `0.0` to skip the AutoUniverse big-loss-pct fast path.
///
/// `app` is optional; when provided, a `auto-universe-blacklisted`
/// event is emitted to the UI on auto-blacklist transitions.
///
/// Lock acquisition: trading(4) inside `update_loss_streak`, then
/// config(1) → market(2) → trading(4) for the AutoUniverse path.
/// All Tier-A; no Tier-B locks held.
pub fn record_trade_outcome(
    state: &TitanState,
    symbol: &str,
    pnl: f64,
    allocated_usd: f64,
    app: Option<&tauri::AppHandle>,
) {
    update_loss_streak(state, symbol, pnl);

    let (au_enabled, au_cfg) = {
        let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
        (cfg.auto_universe.enabled, cfg.auto_universe.clone())
    };
    if !au_enabled {
        return;
    }

    let key = symbol.trim().to_uppercase();
    if key.is_empty() {
        return;
    }

    // Read the *current* streak (just updated above) so the
    // AutoUniverse blacklist threshold check sees a consistent
    // counter. Tier-A trading(4) acquired alone.
    let current_streak: u32 = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading
            .symbol_loss_streak
            .get(&key)
            .map(|(n, _)| *n)
            .unwrap_or(0)
    };

    let report = {
        let mut market = state.market.lock().unwrap_or_else(|p| p.into_inner());
        market.auto_universe.record_trade_outcome(
            &key,
            pnl,
            allocated_usd,
            current_streak,
            &au_cfg,
            Utc::now(),
        )
    };

    if report.now_blacklisted {
        tracing::warn!(
            component = "auto_universe",
            symbol = %report.symbol,
            pnl = format!("{pnl:+.2}"),
            allocated_usd = format!("{allocated_usd:.2}"),
            strikes = report.new_loss_strikes,
            reason = ?report.blacklist_reason,
            "AUTO-BLACKLISTED"
        );
        if let Some(app) = app {
            use tauri::Manager;
            let _ = app.emit_all("auto-universe-blacklisted", &report);
        }
    }
}

/// S17.1 — update the per-symbol intraday loss-streak counter.
///
/// Must be called *outside* the journal lock to honour the global
/// trading(4) → journal(6) ordering. Idempotent on duplicate calls
/// only insofar as `last_event_ts` is monotonic; callers should
/// invoke this exactly once per realised close, gated on the boolean
/// return of `record_trade_close`.
///
/// A non-positive `pnl` (loss or break-even) increments the streak; a
/// positive `pnl` (win) zeroes it. The `entry.rs` Gate 2 reads this
/// counter and refuses new entries on symbols whose streak meets or
/// exceeds `cfg.smarttrend.intraday_loss_blacklist_strikes` (when the
/// gate is enabled, i.e. the threshold is > 0).
///
/// **Prefer `record_trade_outcome` over this function** when calling
/// from a realised-close path — that wrapper also feeds AutoUniverse.
pub fn update_loss_streak(state: &TitanState, symbol: &str, pnl: f64) {
    let key = symbol.trim().to_uppercase();
    if key.is_empty() {
        return;
    }
    let mut trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
    let now = Utc::now().timestamp();
    let entry = trading
        .symbol_loss_streak
        .entry(key.clone())
        .or_insert((0, now));
    if pnl > 0.0 {
        if entry.0 > 0 {
            tracing::debug!(
                component = "journal",
                symbol = %key,
                streak = entry.0,
                "Loss streak reset by win"
            );
        }
        entry.0 = 0;
    } else {
        entry.0 = entry.0.saturating_add(1);
        tracing::info!(
            component = "journal",
            symbol = %key,
            streak = entry.0,
            pnl = format!("{pnl:+.2}"),
            "Intraday loss streak incremented"
        );
    }
    entry.1 = now;
}

// ═══════════════════════════════════════════════════════════════════════
//  Persistence: save / load with hash-skip
// ═══════════════════════════════════════════════════════════════════════

/// Save journal entries atomically with content-hash skip.
pub fn save_journal(state: &TitanState) -> TitanResult<()> {
    let (entries, dirty) = {
        let journal = state.journal.lock().unwrap();
        (journal.entries.clone(), journal.dirty)
    };

    if !dirty {
        return Ok(());
    }

    let json = serde_json::to_string_pretty(&entries)?;
    let hash = crate::ops::persistence::content_hash(json.as_bytes());

    let hash_file = journal_hash_path();
    if let Ok(prev) = fs::read_to_string(&hash_file) {
        if prev.trim() == hash {
            let mut journal = state.journal.lock().unwrap();
            journal.dirty = false;
            return Ok(());
        }
    }

    maybe_archive_and_prune(&entries);

    save_atomic(&journal_path(), &entries)?;
    save_hash_atomic(&hash_file, &hash)?;

    {
        let mut journal = state.journal.lock().unwrap();
        journal.dirty = false;
    }

    tracing::debug!(
        component = "journal",
        entries = entries.len(),
        "Journal saved (hash={hash})"
    );
    Ok(())
}

/// Load journal entries from disk. Returns empty vec if missing/corrupt.
pub fn load_journal() -> Vec<JournalEntry> {
    load_with_fallback(&journal_path())
}

// ═══════════════════════════════════════════════════════════════════════
//  Performance stats
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct PerfStats {
    pub total_trades: usize,
    pub closed_trades: usize,
    pub winners: usize,
    pub losers: usize,
    pub win_rate: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub profit_factor: f64,
    pub total_pnl: f64,
    pub best_trade: f64,
    pub worst_trade: f64,
    pub max_drawdown: f64,
    pub avg_hold_secs: f64,
    pub current_streak: i32,
}

pub fn compute_performance_stats(entries: &[JournalEntry]) -> PerfStats {
    let closed: Vec<&JournalEntry> = entries
        .iter()
        .filter(|e| e.pnl.is_some() && e.exit_time.is_some())
        .collect();

    if closed.is_empty() {
        return PerfStats {
            total_trades: entries.len(),
            closed_trades: 0,
            winners: 0,
            losers: 0,
            win_rate: 0.0,
            avg_win: 0.0,
            avg_loss: 0.0,
            profit_factor: 0.0,
            total_pnl: 0.0,
            best_trade: 0.0,
            worst_trade: 0.0,
            max_drawdown: 0.0,
            avg_hold_secs: 0.0,
            current_streak: 0,
        };
    }

    let mut wins = Vec::new();
    let mut losses = Vec::new();
    let mut total_pnl = 0.0;
    let mut best = f64::NEG_INFINITY;
    let mut worst = f64::INFINITY;
    let mut total_hold = 0i64;
    let mut equity_peak = 0.0_f64;
    let mut max_dd = 0.0_f64;
    let mut cumulative = 0.0;

    for e in &closed {
        let pnl = e.pnl.unwrap();
        total_pnl += pnl;
        cumulative += pnl;

        if pnl >= 0.0 {
            wins.push(pnl);
        } else {
            losses.push(pnl);
        }

        if pnl > best {
            best = pnl;
        }
        if pnl < worst {
            worst = pnl;
        }

        equity_peak = equity_peak.max(cumulative);
        let dd = equity_peak - cumulative;
        max_dd = max_dd.max(dd);

        if let Some(exit_t) = e.exit_time {
            total_hold += exit_t - e.entry_time;
        }
    }

    let avg_win = if wins.is_empty() {
        0.0
    } else {
        wins.iter().sum::<f64>() / wins.len() as f64
    };

    let avg_loss = if losses.is_empty() {
        0.0
    } else {
        losses.iter().sum::<f64>() / losses.len() as f64
    };

    let gross_profit: f64 = wins.iter().sum();
    let gross_loss: f64 = losses.iter().map(|l| l.abs()).sum();
    let profit_factor = if gross_loss > 0.0 {
        gross_profit / gross_loss
    } else if gross_profit > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };

    let avg_hold = if closed.is_empty() {
        0.0
    } else {
        total_hold as f64 / closed.len() as f64
    };

    let win_rate = if closed.is_empty() {
        0.0
    } else {
        wins.len() as f64 / closed.len() as f64 * 100.0
    };

    let streak = compute_streak(&closed);

    PerfStats {
        total_trades: entries.len(),
        closed_trades: closed.len(),
        winners: wins.len(),
        losers: losses.len(),
        win_rate,
        avg_win,
        avg_loss,
        profit_factor,
        total_pnl,
        best_trade: if best == f64::NEG_INFINITY { 0.0 } else { best },
        worst_trade: if worst == f64::INFINITY { 0.0 } else { worst },
        max_drawdown: max_dd,
        avg_hold_secs: avg_hold,
        current_streak: streak,
    }
}

/// Positive = win streak count, negative = loss streak count.
fn compute_streak(closed: &[&JournalEntry]) -> i32 {
    let mut streak = 0i32;
    for e in closed.iter().rev() {
        let pnl = e.pnl.unwrap_or(0.0);
        if streak == 0 {
            streak = if pnl >= 0.0 { 1 } else { -1 };
        } else if (streak > 0 && pnl >= 0.0) || (streak < 0 && pnl < 0.0) {
            streak += streak.signum();
        } else {
            break;
        }
    }
    streak
}

// ═══════════════════════════════════════════════════════════════════════
//  Equity curve for chart
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct EquityPoint {
    pub time: i64,
    pub value: f64,
}

pub fn compute_equity_curve(entries: &[JournalEntry]) -> Vec<EquityPoint> {
    let mut closed: Vec<&JournalEntry> = entries
        .iter()
        .filter(|e| e.pnl.is_some() && e.exit_time.is_some())
        .collect();

    closed.sort_by_key(|e| e.exit_time.unwrap_or(0));

    let mut cumulative = 0.0;
    let mut points = Vec::with_capacity(closed.len() + 1);

    if let Some(first) = closed.first() {
        points.push(EquityPoint {
            time: first.entry_time,
            value: 0.0,
        });
    }

    for e in &closed {
        cumulative += e.pnl.unwrap();
        points.push(EquityPoint {
            time: e.exit_time.unwrap(),
            value: cumulative,
        });
    }

    points
}

// ═══════════════════════════════════════════════════════════════════════
//  CSV export
// ═══════════════════════════════════════════════════════════════════════

pub fn export_csv(entries: &[JournalEntry]) -> TitanResult<String> {
    let ts = Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("trades_{ts}.csv");
    let path = paths::journal_dir().join(&filename);

    let mut file = fs::File::create(&path)?;

    writeln!(
        file,
        "trade_id,symbol,side,entry_price,exit_price,qty,pnl,entry_time,exit_time,exit_reason,entry_score,fsm_phase,strategy_id"
    )?;

    for e in entries {
        let side = match e.side {
            Direction::Long => "Long",
            Direction::Short => "Short",
        };
        let exit_price = e.exit_price.map_or(String::new(), |p| format!("{p:.4}"));
        let pnl = e.pnl.map_or(String::new(), |p| format!("{p:.2}"));
        let exit_time = e
            .exit_time
            .map_or(String::new(), format_timestamp);
        let exit_reason = e.exit_reason.as_deref().unwrap_or("");
        let phase = format!("{:?}", e.fsm_phase_at_entry);
        // P0-C (audit 2026-05-29) — per-strategy attribution column so the
        // CSV export is analysable by strategy (the JSON already carries
        // strategy_id; the CSV silently dropped it, forcing manual JSON
        // parsing for any per-strategy P/L breakdown).
        let strategy_id = e.strategy_id.as_deref().unwrap_or("");

        writeln!(
            file,
            "{},{},{},{:.4},{},{:.4},{},{},{},{},{:.2},{},{}",
            e.trade_id,
            e.symbol,
            side,
            e.entry_price,
            exit_price,
            e.qty,
            pnl,
            format_timestamp(e.entry_time),
            exit_time,
            exit_reason,
            e.entry_score,
            phase,
            strategy_id,
        )?;
    }

    tracing::info!(
        component = "journal",
        path = %path.display(),
        entries = entries.len(),
        "CSV exported"
    );

    Ok(path.to_string_lossy().to_string())
}

fn format_timestamp(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map_or_else(|| ts.to_string(), |dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

// ═══════════════════════════════════════════════════════════════════════
//  Daily P/L breakdown
// ═══════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct DailyPnl {
    pub date: String,
    pub pnl: f64,
    pub trades: usize,
    pub wins: usize,
}

pub fn compute_daily_pnl(entries: &[JournalEntry]) -> Vec<DailyPnl> {
    use std::collections::BTreeMap;

    let mut days: BTreeMap<String, (f64, usize, usize)> = BTreeMap::new();

    for e in entries.iter().filter(|e| e.pnl.is_some() && e.exit_time.is_some()) {
        let date = Utc
            .timestamp_opt(e.exit_time.unwrap(), 0)
            .single()
            .map_or_else(|| "unknown".to_string(), |dt| dt.format("%Y-%m-%d").to_string());

        let pnl = e.pnl.unwrap();
        let entry = days.entry(date).or_insert((0.0, 0, 0));
        entry.0 += pnl;
        entry.1 += 1;
        if pnl >= 0.0 {
            entry.2 += 1;
        }
    }

    days.into_iter()
        .map(|(date, (pnl, trades, wins))| DailyPnl {
            date,
            pnl,
            trades,
            wins,
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(symbol: &str, pnl: Option<f64>, entry_t: i64, exit_t: Option<i64>) -> JournalEntry {
        JournalEntry {
            conviction: None,
            regime: None,
            trade_id: uuid::Uuid::new_v4().to_string(),
            symbol: symbol.to_string(),
            side: Direction::Long,
            entry_price: 100.0,
            exit_price: pnl.map(|p| 100.0 + p),
            qty: 10.0,
            pnl,
            entry_time: entry_t,
            exit_time: exit_t,
            exit_reason: pnl.map(|_| "SignalExit".to_string()),
            entry_score: 65.0,
            fsm_phase_at_entry: FsmPhase::Active,
            strategy_id: None,
            timeframe: "5m".into(),
        }
    }

    #[test]
    fn stats_empty() {
        let stats = compute_performance_stats(&[]);
        assert_eq!(stats.total_trades, 0);
        assert_eq!(stats.closed_trades, 0);
        assert!((stats.win_rate).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_single_winner() {
        let entries = vec![make_entry("AAPL", Some(500.0), 1000, Some(2000))];
        let stats = compute_performance_stats(&entries);
        assert_eq!(stats.closed_trades, 1);
        assert_eq!(stats.winners, 1);
        assert_eq!(stats.losers, 0);
        assert!((stats.win_rate - 100.0).abs() < f64::EPSILON);
        assert!((stats.total_pnl - 500.0).abs() < f64::EPSILON);
        assert!((stats.best_trade - 500.0).abs() < f64::EPSILON);
        assert!((stats.avg_hold_secs - 1000.0).abs() < f64::EPSILON);
        assert_eq!(stats.current_streak, 1);
    }

    #[test]
    fn stats_single_loser() {
        let entries = vec![make_entry("TSLA", Some(-200.0), 1000, Some(1500))];
        let stats = compute_performance_stats(&entries);
        assert_eq!(stats.winners, 0);
        assert_eq!(stats.losers, 1);
        assert!((stats.win_rate).abs() < f64::EPSILON);
        assert!((stats.worst_trade - (-200.0)).abs() < f64::EPSILON);
        assert_eq!(stats.current_streak, -1);
    }

    #[test]
    fn stats_mixed_trades() {
        let entries = vec![
            make_entry("AAPL", Some(300.0), 1000, Some(2000)),
            make_entry("TSLA", Some(-100.0), 2000, Some(3000)),
            make_entry("NVDA", Some(200.0), 3000, Some(4000)),
            make_entry("MSFT", Some(150.0), 4000, Some(5000)),
        ];
        let stats = compute_performance_stats(&entries);
        assert_eq!(stats.closed_trades, 4);
        assert_eq!(stats.winners, 3);
        assert_eq!(stats.losers, 1);
        assert!((stats.win_rate - 75.0).abs() < f64::EPSILON);
        assert!((stats.total_pnl - 550.0).abs() < f64::EPSILON);
        assert!((stats.best_trade - 300.0).abs() < f64::EPSILON);
        assert!((stats.worst_trade - (-100.0)).abs() < f64::EPSILON);
        let avg_win = (300.0 + 200.0 + 150.0) / 3.0;
        assert!((stats.avg_win - avg_win).abs() < 0.01);
        assert!((stats.avg_loss - (-100.0)).abs() < f64::EPSILON);
        assert_eq!(stats.current_streak, 2);
    }

    #[test]
    fn stats_profit_factor() {
        let entries = vec![
            make_entry("A", Some(400.0), 1000, Some(2000)),
            make_entry("B", Some(-200.0), 2000, Some(3000)),
        ];
        let stats = compute_performance_stats(&entries);
        assert!((stats.profit_factor - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_max_drawdown() {
        let entries = vec![
            make_entry("A", Some(500.0), 1000, Some(2000)),
            make_entry("B", Some(-300.0), 2000, Some(3000)),
            make_entry("C", Some(-400.0), 3000, Some(4000)),
            make_entry("D", Some(100.0), 4000, Some(5000)),
        ];
        let stats = compute_performance_stats(&entries);
        // Peak after A = 500, then drops to 500-300-400 = -200, dd = 500 - (-200) = 700
        assert!((stats.max_drawdown - 700.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_open_trades_excluded() {
        let entries = vec![
            make_entry("AAPL", Some(100.0), 1000, Some(2000)),
            make_entry("OPEN", None, 3000, None),
        ];
        let stats = compute_performance_stats(&entries);
        assert_eq!(stats.total_trades, 2);
        assert_eq!(stats.closed_trades, 1);
    }

    #[test]
    fn equity_curve_sorted() {
        let entries = vec![
            make_entry("A", Some(100.0), 1000, Some(3000)),
            make_entry("B", Some(-50.0), 500, Some(2000)),
        ];
        let curve = compute_equity_curve(&entries);
        assert_eq!(curve.len(), 3); // anchor + 2 points
        assert!(curve[1].time <= curve[2].time);
        assert!((curve[1].value - (-50.0)).abs() < f64::EPSILON); // B closes first
        assert!((curve[2].value - 50.0).abs() < f64::EPSILON); // A closes second
    }

    #[test]
    fn equity_curve_empty() {
        let curve = compute_equity_curve(&[]);
        assert!(curve.is_empty());
    }

    #[test]
    fn csv_export_creates_file() {
        let entries = vec![make_entry("AAPL", Some(100.0), 1000, Some(2000))];
        let dir = paths::journal_dir();
        fs::create_dir_all(&dir).ok();
        let result = export_csv(&entries);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(std::path::Path::new(&path).exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("AAPL"));
        assert!(content.contains("Long"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn daily_pnl_grouping() {
        // A,B exit on 2023-11-14 UTC; C exits on 2023-11-16 UTC
        let entries = vec![
            make_entry("A", Some(100.0), 1700000000, Some(1700001000)),
            make_entry("B", Some(-50.0), 1700000000, Some(1700002000)),
            make_entry("C", Some(200.0), 1700100000, Some(1700103600)),
        ];
        let daily = compute_daily_pnl(&entries);
        assert_eq!(daily.len(), 2);
        let first = &daily[0];
        assert_eq!(first.trades, 2);
        assert!((first.pnl - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn streak_all_wins() {
        let entries: Vec<JournalEntry> = (0..5)
            .map(|i| make_entry("X", Some(100.0), i * 1000, Some(i * 1000 + 500)))
            .collect();
        let stats = compute_performance_stats(&entries);
        assert_eq!(stats.current_streak, 5);
    }

    #[test]
    fn streak_alternating() {
        let entries = vec![
            make_entry("A", Some(100.0), 1000, Some(2000)),
            make_entry("B", Some(-50.0), 2000, Some(3000)),
        ];
        let stats = compute_performance_stats(&entries);
        assert_eq!(stats.current_streak, -1);
    }

    /// S16.5 — `record_trade_close` must fall back to symbol match even
    /// when a non-empty trade_id is supplied but doesn't exist in the
    /// journal. This bridges the gap between `position_recon`-created
    /// entries (synthetic id) and `trade_updates`-driven closes (broker
    /// SELL order id).
    #[test]
    fn s16_5_record_trade_close_falls_back_to_symbol_when_trade_id_misses() {
        use crate::config::RuntimeConfig;
        use crate::state::{Direction, FsmPhase, TitanState};

        let state = TitanState::new(RuntimeConfig::default());
        record_trade_open(
            &state,
            TradeOpenParams {
                conviction: None,
                regime: None,
                trade_id: "recon-LWLG-1777664148".into(),
                symbol: "LWLG".into(),
                side: Direction::Long,
                entry_price: 16.08,
                qty: 31.0,
                entry_time: 1_700_000_000,
                entry_score: 0.0,
                fsm_phase: FsmPhase::Active,
                strategy_id: None,
                timeframe: String::new(),
            },
        );

        // Close arrives with a *different* trade_id (broker SELL order id).
        record_trade_close(
            &state,
            "65f4e815-1230-4a81-ae09-42131a42d9e6",
            "LWLG",
            16.22,
            4.34,
            "SignalExit",
        );

        let entries = state.journal.lock().unwrap().entries.clone();
        assert_eq!(entries.len(), 1, "no duplicate journal entry created");
        let entry = &entries[0];
        assert!(entry.exit_time.is_some(), "entry must be marked closed via symbol fallback");
        assert_eq!(entry.exit_price, Some(16.22));
        assert_eq!(entry.pnl, Some(4.34));
        assert_eq!(entry.exit_reason.as_deref(), Some("SignalExit"));
    }

    /// S16.4 — `record_trade_open` must skip when an OPEN entry for the
    /// same symbol already exists. Prevents `position_recon` from
    /// accumulating one phantom row per restart for the same broker
    /// position.
    #[test]
    fn s16_4_record_trade_open_dedup_for_open_position() {
        use crate::config::RuntimeConfig;
        use crate::state::{Direction, FsmPhase, TitanState};

        let state = TitanState::new(RuntimeConfig::default());
        let params = || TradeOpenParams {
            conviction: None,
            regime: None,
            trade_id: format!("recon-AAPL-{}", chrono::Utc::now().timestamp()),
            symbol: "AAPL".into(),
            side: Direction::Long,
            entry_price: 180.0,
            qty: 10.0,
            entry_time: 1_700_000_000,
            entry_score: 0.0,
            fsm_phase: FsmPhase::Active,
            strategy_id: None,
            timeframe: String::new(),
        };

        record_trade_open(&state, params());
        record_trade_open(&state, params());
        record_trade_open(&state, params());

        let entries = state.journal.lock().unwrap().entries.clone();
        let open_aapl: Vec<_> = entries
            .iter()
            .filter(|e| e.symbol == "AAPL" && e.exit_time.is_none())
            .collect();
        assert_eq!(
            open_aapl.len(),
            1,
            "should keep exactly 1 OPEN entry per symbol regardless of repeated opens"
        );

        // After closing, a fresh open is allowed (e.g., user re-enters next day).
        record_trade_close(&state, &open_aapl[0].trade_id, "AAPL", 195.0, 150.0, "TakeProfit");
        record_trade_open(&state, params());
        let entries = state.journal.lock().unwrap().entries.clone();
        let aapl_total: Vec<_> = entries.iter().filter(|e| e.symbol == "AAPL").collect();
        assert_eq!(
            aapl_total.len(),
            2,
            "after close, a new open is permitted so the second cycle is journalled"
        );
    }

    #[test]
    fn journal_entry_serde() {
        let entry = make_entry("AAPL", Some(250.0), 1700000000, Some(1700001000));
        let json = serde_json::to_string(&entry).unwrap();
        let loaded: JournalEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.symbol, "AAPL");
        assert!((loaded.pnl.unwrap() - 250.0).abs() < f64::EPSILON);
        assert_eq!(loaded.fsm_phase_at_entry, FsmPhase::Active);
    }

    fn make_open_entry(symbol: &str, trade_id: &str, entry_t: i64) -> JournalEntry {
        JournalEntry {
            conviction: None,
            regime: None,
            trade_id: trade_id.to_string(),
            symbol: symbol.to_string(),
            side: Direction::Long,
            entry_price: 100.0,
            exit_price: None,
            qty: 10.0,
            pnl: None,
            entry_time: entry_t,
            exit_time: None,
            exit_reason: None,
            entry_score: 65.0,
            fsm_phase_at_entry: FsmPhase::Active,
            strategy_id: None,
            timeframe: String::new(),
        }
    }

    #[test]
    fn journal_record_close_matches_by_trade_id_not_symbol() {
        use crate::config::RuntimeConfig;
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut journal = state.journal.lock().unwrap();
            journal.entries.push(make_open_entry("AAPL", "order-1", 1000));
            journal.entries.push(make_open_entry("AAPL", "order-2", 2000));
        }

        record_trade_close(&state, "order-1", "AAPL", 110.0, 100.0, "SignalExit");

        let journal = state.journal.lock().unwrap();
        let e1 = journal.entries.iter().find(|e| e.trade_id == "order-1").unwrap();
        assert!(e1.exit_time.is_some(), "order-1 should be closed");
        let e2 = journal.entries.iter().find(|e| e.trade_id == "order-2").unwrap();
        assert!(e2.exit_time.is_none(), "order-2 should remain open");
    }

    #[test]
    fn journal_record_close_falls_back_to_symbol_when_trade_id_empty() {
        use crate::config::RuntimeConfig;
        let state = TitanState::new(RuntimeConfig::default());
        {
            let mut journal = state.journal.lock().unwrap();
            journal.entries.push(make_open_entry("TSLA", "tid-legacy", 1000));
        }

        record_trade_close(&state, "", "TSLA", 220.0, 200.0, "ManualClose");

        let journal = state.journal.lock().unwrap();
        let e = journal.entries.iter().find(|e| e.symbol == "TSLA").unwrap();
        assert!(e.exit_time.is_some(), "should match by symbol fallback");
    }
}
