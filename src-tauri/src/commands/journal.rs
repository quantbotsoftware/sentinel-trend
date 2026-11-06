use std::sync::Arc;

use serde::Serialize;

use crate::ops::journal::{
    compute_daily_pnl, compute_equity_curve, compute_performance_stats, export_csv, DailyPnl,
    EquityPoint, PerfStats,
};
use crate::state::{JournalEntry, TitanState};

/// v2.5.1 HF — equity-based snapshot served alongside the realized
/// `PerfStats` so the JOURNAL tab can show what Alpaca sees (daily
/// equity change), not just the sum of closed trades.
///
/// **Why this exists**: live evidence 2026-05-18 — JOURNAL displayed
/// `PnL: +$234.84` while Alpaca's dashboard reported `Daily Change:
/// +$118.58`. Both numbers were correct but measured different things:
/// the journal sums realized P/L on closed trades, while Alpaca's
/// figure is `equity_now - equity_open_today` (includes mark-to-market
/// drift on still-open positions and overnight cost-basis vs current
/// market value for reconciled positions). Operating discipline
/// requires the broker's number to be visible too — that's the cash
/// you actually have.
#[derive(Debug, Serialize)]
pub struct AccountSummary {
    /// Total account equity right now (cash + market value of all
    /// open positions, longs positive, shorts negative). Matches the
    /// "Portfolio" line on the Alpaca dashboard.
    pub session_equity: f64,
    /// S19 — broker-side anchor for the daily change: Alpaca's
    /// `last_equity` (equity at the previous session close, set by
    /// the daily close-of-business rollover). Mirrored verbatim from
    /// `risk.alpaca_last_equity`. Stays constant across the entire
    /// session so the daily figure doesn't drift when the bot
    /// restarts mid-day.
    pub last_equity: f64,
    /// Equity captured at the start of the current trading day by
    /// the bot's internal tracking. Refreshed by `equity_sync` at
    /// session open. Kept in the payload for diagnostics — UI uses
    /// `daily_equity_change` (computed against `last_equity`) so
    /// the displayed number always matches Alpaca.
    pub initial_equity: f64,
    /// **The number that matches Alpaca's "Daily Change" verbatim.**
    /// Computed as `session_equity - last_equity` when `last_equity`
    /// is known, falling back to `session_equity - initial_equity`
    /// only if Alpaca hasn't returned `last_equity` yet (e.g. fresh
    /// install, first sync still in flight). The fallback eventually
    /// agrees once equity_sync runs at least once.
    pub daily_equity_change: f64,
    /// Available cash (settled). Matches Alpaca "Cash".
    pub cash: f64,
    /// Margin / buying power per Alpaca account endpoint.
    pub buying_power: f64,
    /// Sum of `unrealized_pnl` across all open positions (real-time
    /// mark-to-market). Adding this to `realized_total` does NOT
    /// equal `daily_equity_change` because positions held overnight
    /// have a cost-basis from a prior day; the residual delta is the
    /// overnight P/L absorbed into today's equity.
    pub unrealized_total: f64,
    /// Mirror of `stats.total_pnl` for clarity — sum of realized P/L
    /// on closed trades only.
    pub realized_total: f64,
}

#[derive(Debug, Serialize)]
pub struct JournalResponse {
    pub entries: Vec<JournalEntry>,
    pub stats: PerfStats,
    pub daily_pnl: Vec<DailyPnl>,
    pub total: Option<usize>,
    pub account: AccountSummary,
}

#[tauri::command]
pub fn get_journal(
    state: tauri::State<'_, Arc<TitanState>>,
    from: Option<usize>,
    limit: Option<usize>,
) -> JournalResponse {
    let entries = {
        let journal = state.journal.lock().unwrap();
        journal.entries.clone()
    };

    let stats = compute_performance_stats(&entries);
    let daily_pnl = compute_daily_pnl(&entries);

    // v2.5.1 HF — pull the equity-based snapshot from `state.risk`
    // and the unrealized total from `state.trading.positions`.
    //
    // Lock order: risk(5) -> trading(4) is a downward acquire which
    // VIOLATES our ascending convention. We therefore snapshot each
    // independently with a short critical section, dropping the lock
    // before acquiring the next. No inversion can occur because we
    // never hold both simultaneously.
    let (session_equity, initial_equity, last_equity, cash, buying_power) = {
        let risk = state.risk.lock().unwrap_or_else(|p| p.into_inner());
        (
            risk.session_equity,
            risk.initial_equity,
            risk.alpaca_last_equity,
            risk.cash,
            risk.buying_power,
        )
    };
    let unrealized_total: f64 = {
        let trading = state.trading.lock().unwrap_or_else(|p| p.into_inner());
        trading.positions.iter().map(|p| p.unrealized_pnl).sum()
    };
    // S19 — anchor daily change to Alpaca's `last_equity` so a
    // mid-day restart doesn't reset the displayed figure. Fall back
    // to the internal `initial_equity` only when last_equity hasn't
    // been populated yet (e.g. fresh install, first sync pending).
    let daily_equity_change = if last_equity > 0.0 {
        session_equity - last_equity
    } else if initial_equity > 0.0 {
        session_equity - initial_equity
    } else {
        0.0
    };
    let account = AccountSummary {
        session_equity,
        last_equity,
        initial_equity,
        daily_equity_change,
        cash,
        buying_power,
        unrealized_total,
        realized_total: stats.total_pnl,
    };

    let mut sorted = entries;
    sorted.sort_by(|a, b| b.entry_time.cmp(&a.entry_time));

    let total = sorted.len();
    let from = from.unwrap_or(0);
    let limit = limit.unwrap_or(200).min(1000);
    let paged: Vec<JournalEntry> = sorted.into_iter().skip(from).take(limit).collect();

    JournalResponse {
        entries: paged,
        stats,
        daily_pnl,
        total: Some(total),
        account,
    }
}

#[tauri::command]
pub fn get_equity_curve(
    state: tauri::State<'_, Arc<TitanState>>,
) -> Vec<EquityPoint> {
    let entries = {
        let journal = state.journal.lock().unwrap();
        journal.entries.clone()
    };
    compute_equity_curve(&entries)
}

#[tauri::command]
pub fn export_journal_csv(
    state: tauri::State<'_, Arc<TitanState>>,
) -> Result<String, String> {
    let entries = {
        let journal = state.journal.lock().unwrap();
        journal.entries.clone()
    };
    export_csv(&entries).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_response_serialize() {
        let resp = JournalResponse {
            entries: vec![],
            stats: compute_performance_stats(&[]),
            daily_pnl: vec![],
            total: Some(0),
            account: AccountSummary {
                session_equity: 0.0,
                last_equity: 0.0,
                initial_equity: 0.0,
                daily_equity_change: 0.0,
                cash: 0.0,
                buying_power: 0.0,
                unrealized_total: 0.0,
                realized_total: 0.0,
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("total_trades"));
        assert!(json.contains("win_rate"));
        assert!(json.contains("\"total\":0"));
        // v2.5.1 HF — the equity-based account snapshot must be in
        // the wire payload so the UI can render Alpaca-source figures.
        assert!(
            json.contains("daily_equity_change"),
            "AccountSummary must be serialized into JournalResponse"
        );
    }

    /// Helper mirroring the production daily_equity_change ladder
    /// so tests can pin the exact fallback semantics.
    fn compute_daily_change(session: f64, last_equity: f64, initial: f64) -> f64 {
        if last_equity > 0.0 {
            session - last_equity
        } else if initial > 0.0 {
            session - initial
        } else {
            0.0
        }
    }

    #[test]
    fn account_summary_uses_alpaca_last_equity_when_available() {
        // S19 live 2026-05-18: Alpaca showed +$121.39 daily change
        // while the bot's internal initial_equity had been clobbered
        // by a mid-day daily_reset, displaying only +$16.15. With
        // last_equity pinned to Alpaca's reported value, the
        // computation matches the broker exactly.
        let session = 100_121.39;
        let last_equity = 100_000.00; // from Alpaca account endpoint
        let initial = 100_105.24;      // stale internal anchor
        let change = compute_daily_change(session, last_equity, initial);
        assert!((change - 121.39).abs() < 1e-6, "got {change}");
    }

    #[test]
    fn account_summary_falls_back_to_initial_equity_when_last_equity_absent() {
        // Fresh install / first-sync-in-flight: last_equity still
        // zero so use the internal initial_equity as a best-effort
        // ancor. The fallback converges to the Alpaca-aligned value
        // on the next equity_sync tick.
        let session = 100_500.00;
        let last_equity = 0.0;
        let initial = 100_000.00;
        let change = compute_daily_change(session, last_equity, initial);
        assert!((change - 500.0).abs() < 1e-9);
    }

    #[test]
    fn account_summary_yields_zero_change_when_no_anchor_known() {
        // Boot-time / weekend safety: both anchors still zero.
        // Don't manufacture a phantom -$100k "change".
        let change = compute_daily_change(100_000.0, 0.0, 0.0);
        assert_eq!(change, 0.0);
    }

    #[test]
    fn journal_pagination_returns_correct_window() {
        use crate::config::RuntimeConfig;
        use crate::state::{Direction, FsmPhase, JournalEntry};

        let state = std::sync::Arc::new(crate::state::TitanState::new(RuntimeConfig::default()));
        {
            let mut journal = state.journal.lock().unwrap();
            for i in 0..50 {
                journal.entries.push(JournalEntry {
                    conviction: None,
                    regime: None,
                    trade_id: format!("t{i}"),
                    symbol: "AAPL".into(),
                    side: Direction::Long,
                    entry_price: 100.0,
                    exit_price: Some(110.0),
                    qty: 1.0,
                    pnl: Some(10.0),
                    entry_time: i as i64 * 1000,
                    exit_time: Some(i as i64 * 1000 + 500),
                    exit_reason: Some("Exit".into()),
                    entry_score: 60.0,
                    fsm_phase_at_entry: FsmPhase::Active,
                    strategy_id: None,
                    timeframe: "5m".into(),
                });
            }
        }

        let entries_full = {
            let journal = state.journal.lock().unwrap();
            journal.entries.clone()
        };
        let stats = compute_performance_stats(&entries_full);
        let _daily_pnl = compute_daily_pnl(&entries_full);

        let mut sorted = entries_full;
        sorted.sort_by(|a, b| b.entry_time.cmp(&a.entry_time));
        let total = sorted.len();
        let from = 10_usize;
        let limit = 20_usize;
        let paged: Vec<JournalEntry> = sorted.into_iter().skip(from).take(limit).collect();

        assert_eq!(paged.len(), 20);
        assert_eq!(total, 50);
        assert_eq!(stats.closed_trades, 50);
    }
}
