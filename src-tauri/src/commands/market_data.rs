use std::sync::Arc;

use chrono::TimeZone;
use serde::Serialize;
use titan_types::Candle;

use crate::error::TitanError;
use crate::market::candle_buffer::{timeframe_to_secs, CandleRingBuffer};
use crate::market::market_state::MarketDataState;
use crate::market::streamer::StreamCmd;
use crate::state::{normalize_tf, BarClosedMsg, Direction, MarkerKind, TitanState};

#[derive(Debug, Clone, Serialize)]
pub struct ChartDataResponse {
    pub candles: Vec<Candle>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChartMarker {
    pub time: i64,
    pub position: &'static str,
    pub color: &'static str,
    pub shape: &'static str,
    /// Lightweight-Charts text label. Use `String` (not `&'static str`)
    /// so REAL trade markers can embed dynamic info (e.g. P/L cents).
    pub text: String,
    pub score: f64,
    pub price: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Marker visual prominence (1 = small / FSM shadow, 2 = bold / REAL trade).
    /// Lightweight-Charts honours `size` natively. Defaults to 1 in the
    /// JSON for backwards compatibility with older snapshots.
    #[serde(default = "default_marker_size")]
    pub size: u8,
    /// S34 (2026-05-28) — `true` when the marker is anchored to a real
    /// executed trade (journal entry). `false` for FSM shadow signals
    /// emitted by the simulator regardless of whether a trade fired.
    /// Frontend uses this to differentiate tooltip text + emphasis.
    pub is_real: bool,
    /// `true` when post-eval gates vetoed the Entry (spread / history /
    /// quality / overextended). Chart MUST render these as SKIP, never
    /// as a tempting BUY — otherwise manual traders treat vetoed arrows
    /// as actionable. Defaults false for journal / legacy payloads.
    #[serde(default)]
    pub display_only: bool,
}

fn default_marker_size() -> u8 {
    1
}

pub(crate) fn normalize_alpaca_tf(tf: &str) -> &'static str {
    match tf {
        "1m" => "1Min",
        "5m" => "5Min",
        "10m" => "10Min",
        "15m" => "15Min",
        "30m" => "30Min",
        "1h" => "1Hour",
        "4h" => "4Hour",
        "1d" | "1D" => "1Day",
        _ => "5Min",
    }
}

pub(crate) fn days_for_timeframe(tf: &str) -> i64 {
    // S35.6 (2026-05-28 17:25 ET) — fetch ranges trimmed to recent
    // action only. Pre-S35.6 the wide ranges (e.g. 1h=90d, 4h=180d,
    // 1D=365d) loaded ancient bars at very different price levels
    // (QBTS at $15 in March vs $30 today). Even with a tight visible
    // window in chart.js, the bars themselves were loaded into the
    // chart and could be revealed by user scroll OR by autoscale
    // miscalculation, creating a vertical "gap" between historic
    // markers and current price.
    //
    // Trade-off: FSM markers older than the fetch window will not be
    // re-generated during replay. Tony confirmed via screenshots
    // (May 28 16:29 ET) that the historic markers (Feb/Mar at $15,
    // Dec at $22) are NOT actionable for live trading at $30 today —
    // they only create visual confusion. We optimize for clean
    // recent-action charts.
    match tf {
        "1m" => 5,
        "5m" => 10,
        "10m" => 10,
        "15m" => 30,    // restored — needed so 15m FSM can replay 30 days
        "30m" => 30,
        "1h" => 60,     // 2 months — enough for FSM to find good transitions
        "4h" => 120,
        "1d" | "1D" => 365,
        _ => 5,
    }
}

/// S35.6 (2026-05-28) — maximum age (in seconds) of markers that
/// should ever be exposed to the chart UI for a given timeframe.
/// Used by `get_markers` to filter out stale / orphaned markers
/// from previous broader fetch ranges that may still live in the
/// snapshot file. Prevents the visual "gap" caused by ancient
/// markers anchoring the chart's price-axis to historic price
/// levels.
pub(crate) fn marker_max_age_secs(tf: &str) -> i64 {
    // S35.7 (2026-05-28 17:35 ET) — relaxed back to give every TF
    // some marker context. Combined with the narrow
    // `TF_DEFAULT_VISIBLE_BARS` in chart.js, ancient markers exist
    // in storage (visible if user scrolls left) but stay off-screen
    // by default so autoScale stays tight on recent action.
    let days = match tf {
        "1m" => 5,    // ~last week of intraday signals
        "5m" => 14,
        "10m" => 14,
        "15m" => 14,
        "30m" => 21,
        "1h" => 30,   // last month
        "4h" => 90,   // last quarter
        "1d" | "1D" => 365,
        _ => 14,
    };
    days * 86400
}

pub(crate) async fn fetch_candles_internal(
    symbol: &str,
    timeframe: &str,
    http_client: &reqwest::Client,
    key: &str,
    secret: &str,
) -> Result<Vec<Candle>, TitanError> {
    let days = days_for_timeframe(timeframe);
    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
    let end_et = now_et.date_naive().and_hms_opt(16, 0, 0).unwrap();
    let start_et = (now_et - chrono::Duration::days(days))
        .date_naive()
        .and_hms_opt(9, 30, 0)
        .unwrap();
    let end = chrono_tz::US::Eastern
        .from_local_datetime(&end_et)
        .single()
        .unwrap()
        .with_timezone(&chrono::Utc);
    let start = chrono_tz::US::Eastern
        .from_local_datetime(&start_et)
        .single()
        .unwrap()
        .with_timezone(&chrono::Utc);
    fetch_candles_range_internal(symbol, timeframe, start, end, http_client, key, secret).await
}

/// Fetch Alpaca bars for an explicit `[start, end]` UTC range. Used by
/// the historical bootstrap pipeline (S4 P0.2). Returns up to 10,000
/// rows per call — caller chunks if their range exceeds that.
///
/// All other params (auth, JSON parsing, error handling) match
/// [`fetch_candles_internal`]; only the date window changes.
pub(crate) async fn fetch_candles_range_internal(
    symbol: &str,
    timeframe: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    http_client: &reqwest::Client,
    key: &str,
    secret: &str,
) -> Result<Vec<Candle>, TitanError> {
    let tf = normalize_alpaca_tf(timeframe);

    tracing::debug!(
        "Fetching history range: {symbol} {tf} {} -> {}",
        start.to_rfc3339(),
        end.to_rfc3339()
    );

    // S41 (audit 2026-05-29) — FOLLOW PAGINATION.
    //
    // Bug: Alpaca's bars endpoint returns AT MOST one page (~1000 bars
    // for the SIP feed regardless of `limit`) and a `next_page_token`
    // when more rows exist. The previous code issued a single request
    // and ignored the token. With `sort=asc` the first page holds the
    // OLDEST bars, so the returned `last()` candle was stale — e.g. for
    // QBTS the 4h chart ended on 2026-02-19 ($19.08) instead of today
    // (~$29.40). This desynced the header price, the price scale, AND
    // the FSM replay (markers were computed on weeks-old data). Each TF
    // truncated at a different date, so every timeframe showed a
    // different "current" price (20.31 / 20.32 / 21.68 / 19.09).
    //
    // Fix: loop on `next_page_token` until exhausted (capped) so the
    // full requested window is returned, ending on the most recent bar.
    let start_rfc = start.to_rfc3339();
    let end_rfc = end.to_rfc3339();
    const MAX_PAGES: usize = 50;
    let mut candles: Vec<Candle> = Vec::new();
    let mut page_token: Option<String> = None;
    let mut pages = 0usize;

    loop {
        let mut query: Vec<(&str, String)> = vec![
            ("symbols", symbol.to_string()),
            ("timeframe", tf.to_string()),
            ("limit", "10000".to_string()),
            ("start", start_rfc.clone()),
            ("end", end_rfc.clone()),
            ("sort", "asc".to_string()),
            ("feed", "sip".to_string()),
            ("adjustment", "all".to_string()),
        ];
        if let Some(tok) = &page_token {
            query.push(("page_token", tok.clone()));
        }

        let res = http_client
            .get("https://data.alpaca.markets/v2/stocks/bars")
            .query(&query)
            .header("APCA-API-KEY-ID", key)
            .header("APCA-API-SECRET-KEY", secret)
            .send()
            .await
            .map_err(|e| TitanError::Network(format!("HTTP request failed: {e}")))?;

        if !res.status().is_success() {
            let status = res.status();
            let req_id = res
                .headers()
                .get("x-request-id")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("?")
                .to_string();
            let body = res.text().await.unwrap_or_default();
            tracing::error!(status = %status, request_id = %req_id, body = %body, "Alpaca API error (history)");
            return Err(TitanError::Broker(format!(
                "Alpaca API {status}: request_id={req_id}"
            )));
        }

        let json: serde_json::Value = res
            .json()
            .await
            .map_err(|e| TitanError::Internal(format!("JSON parse error: {e}")))?;

        let bars_obj = json
            .get("bars")
            .ok_or_else(|| TitanError::Internal("No 'bars' field in response".into()))?;

        let bars_array = bars_obj
            .get(symbol)
            .or_else(|| bars_obj.as_object().and_then(|o| o.values().next()));

        if let Some(bars) = bars_array.and_then(|b| b.as_array()) {
            candles.extend(bars.iter().filter_map(|b| {
                let close = b.get("c").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if close <= 0.0 {
                    return None;
                }
                let t_str = b.get("t").and_then(|v| v.as_str()).unwrap_or("");
                let time = chrono::DateTime::parse_from_rfc3339(t_str)
                    .map(|dt| dt.timestamp())
                    .unwrap_or(0);
                Some(Candle {
                    time,
                    open: b.get("o").and_then(|v| v.as_f64()).unwrap_or(close),
                    high: b.get("h").and_then(|v| v.as_f64()).unwrap_or(close),
                    low: b.get("l").and_then(|v| v.as_f64()).unwrap_or(close),
                    close,
                    volume: b.get("v").and_then(|v| v.as_f64()).unwrap_or(0.0),
                })
            }));
        }

        pages += 1;
        page_token = json
            .get("next_page_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        if page_token.is_none() || pages >= MAX_PAGES {
            if pages >= MAX_PAGES && page_token.is_some() {
                tracing::warn!(
                    "History pagination hit MAX_PAGES={MAX_PAGES} for {symbol} {tf}; \
                     truncating (returned {} bars)",
                    candles.len()
                );
            }
            break;
        }
    }

    tracing::debug!(
        "Fetched {} candles for {symbol} {tf} across {pages} page(s)",
        candles.len()
    );
    Ok(candles)
}

#[tauri::command]
pub async fn fetch_alpaca_history(
    symbol: String,
    timeframe: String,
    state: tauri::State<'_, Arc<TitanState>>,
    market: tauri::State<'_, Arc<MarketDataState>>,
) -> Result<ChartDataResponse, TitanError> {
    let (key, secret) = {
        let cfg = state
            .config
            .lock()
            .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
        (
            cfg.market.alpaca_key_id.clone(),
            cfg.market.alpaca_secret.clone(),
        )
    };

    if key.is_empty() || secret.is_empty() {
        return Err(TitanError::Validation(
            "Alpaca API keys not configured".into(),
        ));
    }

    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err(TitanError::Validation("Symbol cannot be empty".into()));
    }

    let candles = fetch_candles_internal(&symbol, &timeframe, &state.http_client, &key, &secret).await?;

    if !candles.is_empty() {
        let norm = normalize_tf(&timeframe);
        let buf_key = format!("{symbol}:{norm}");
        let tf_secs = timeframe_to_secs(norm);

        let eval_outside = state
            .config
            .lock()
            .map_err(|e| TitanError::LockPoisoned(e.to_string()))?
            .smarttrend
            .eval_outside_hours;

        if eval_outside {
            // Reset FSM and candle buffer for clean replay
            {
                let mut st = state
                    .smarttrend
                    .lock()
                    .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
                // S44 (2026-05-29) — reset the PER-(symbol, TF) FSM.
                //
                // Regression history: S36 re-instated per-TF FSM keying
                // (`fsm_key(symbol, tf)` = "QBTS:1h") in the sentinel
                // dispatch, but this replay reset was left at the S35.6
                // symbol-only key (`remove(&symbol)` = "QBTS"). That made
                // the reset a NO-OP under the new scheme: the per-TF FSM
                // ("QBTS:1h") survived every reload, frozen in its last
                // terminal phase (Active/Exit). Consequence: the FIRST
                // chart load of a TF replayed on a fresh FSM and emitted
                // markers, but EVERY subsequent reload wiped those markers
                // (the `retain` below) while the stale FSM refused to
                // transition Idle→Active again → 0 regenerated markers →
                // a permanently marker-less chart for that TF.
                //
                // Removing the matching per-TF key lets each reload replay
                // from a clean FSM and regenerate markers deterministically.
                st.fsm.remove(&buf_key);
                let before = st.markers.len();
                st.markers
                    .retain(|m| !(m.symbol == symbol && normalize_tf(&m.timeframe) == norm));
                let removed = before.saturating_sub(st.markers.len());
                if removed > 0 {
                    tracing::debug!(
                        "Replay reset removed {removed} stale markers for {symbol} {norm}"
                    );
                }
            }
            {
                let mut buffers = market
                    .candle_buffers
                    .lock()
                    .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
                buffers.remove(&buf_key);
            }

            let tx_guard = state
                .bar_tx
                .lock()
                .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
            if let Some(tx) = tx_guard.as_ref() {
                let tx = tx.clone();
                let sym = symbol.clone();
                let tf = norm.to_string();
                let replay_candles = candles.clone();
                let market_arc = market.inner().clone();
                tokio::spawn(async move {
                    let total = replay_candles.len();
                    let mut sent = 0usize;
                    for c in &replay_candles {
                        {
                            let mut buffers = market_arc.candle_buffers.lock().unwrap();
                            let buf = buffers
                                .entry(format!("{sym}:{tf}"))
                                .or_insert_with(|| CandleRingBuffer::new(tf_secs));
                            buf.push_candle(c.clone());
                        }
                        let msg = BarClosedMsg {
                            symbol: sym.clone(),
                            timeframe: tf.clone(),
                            bar_time: c.time,
                            close_price: c.close,
                        };
                        if tx.send(msg).await.is_err() {
                            break;
                        }
                        sent += 1;
                    }
                    tracing::info!("Replayed {sent}/{total} bars for {sym} {tf}");
                });
            } else {
                tracing::warn!("bar_tx not initialized, cannot trigger sentinel eval");
            }
        } else {
            {
                let mut buffers = market
                    .candle_buffers
                    .lock()
                    .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
                let buf = buffers
                    .entry(buf_key)
                    .or_insert_with(|| CandleRingBuffer::new(tf_secs));
                buf.seed(candles.clone());
            }
            if let Some(last) = candles.last() {
                let msg = BarClosedMsg {
                    symbol: symbol.clone(),
                    timeframe: norm.to_string(),
                    bar_time: last.time,
                    close_price: last.close,
                };
                let tx = state
                    .bar_tx
                    .lock()
                    .map_err(|e| TitanError::LockPoisoned(e.to_string()))?;
                if let Some(tx) = tx.as_ref() {
                    let _ = tx.try_send(msg);
                }
            }
        }
    }

    Ok(ChartDataResponse { candles })
}

#[tauri::command]
pub fn set_active_symbol(
    state: tauri::State<'_, Arc<TitanState>>,
    symbol: String,
) -> Result<(), String> {
    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() {
        return Err("Symbol cannot be empty".to_string());
    }

    {
        let mut market = state.market.lock().map_err(|e| format!("lock: {e}"))?;
        market.active_symbol = symbol.clone();
    }

    let tx = state.stream_tx.lock().map_err(|e| format!("lock: {e}"))?;
    if let Some(tx) = tx.as_ref() {
        tx.try_send(StreamCmd::SetSymbol(symbol.clone()))
            .map_err(|e| format!("Failed to send symbol change: {e}"))?;
    }

    tracing::info!("Active symbol set to {symbol}");
    Ok(())
}

#[tauri::command]
pub fn get_active_symbol(state: tauri::State<'_, Arc<TitanState>>) -> Result<String, String> {
    let market = state.market.lock().map_err(|e| format!("lock: {e}"))?;
    Ok(market.active_symbol.clone())
}

/// Chart markers — trust tiers (F3b):
///   • live-path GO (`source=apex|frontier`, verdict=GO) → actionable label
///   • live-path VETO / SHADOW → muted reference
///   • FSM Entry in apex_only → `FSM` (never `GO`) — does not place orders
///   • HOLD (P0 defer) → gold — winner protected
///   • Exit executed → `GO SELL`
/// Real fills remain ★ BOT via journal.
fn marker_to_chart(m: &crate::state::SmartTrendMarker, compact: bool) -> Option<ChartMarker> {
    let source = m.source.as_deref().unwrap_or("fsm");
    let verdict = m.verdict.as_deref().unwrap_or(if m.display_only {
        "VETO"
    } else if source == "fsm" {
        "FSM"
    } else {
        "GO"
    });
    let conv = m.conviction.unwrap_or(m.score / 100.0);
    let short = matches!(m.direction, crate::state::Direction::Short);
    let setup = m.setup.as_deref();

    match (&m.kind, verdict) {
        (MarkerKind::Exit, "HOLD") => Some(ChartMarker {
            time: m.bar_time,
            position: "aboveBar",
            color: "#D29922",
            shape: "circle",
            text: "HOLD".into(),
            score: m.score,
            price: m.price,
            reason: Some(format!(
                "HOLD — winner protected at +{conv:.2}R; breakeven armed, do NOT sell manually"
            )),
            size: 1,
            is_real: false,
            display_only: true,
        }),
        (MarkerKind::Exit, _) => {
            let text = if compact {
                String::new()
            } else if source != "fsm" {
                "GO SELL".into()
            } else {
                "FSM SELL".into()
            };
            Some(ChartMarker {
                time: m.bar_time,
                position: "aboveBar",
                color: if compact {
                    "rgba(248,81,73,0.40)"
                } else {
                    "#F85149"
                },
                shape: "arrowDown",
                text,
                score: m.score,
                price: m.price,
                reason: Some(
                    "EXIT — flatten here (simulation unless ★ BOT SELL also present).".into(),
                ),
                size: 1,
                is_real: false,
                display_only: false,
            })
        }
        (MarkerKind::Entry, "HOLD") => None,
        (MarkerKind::Entry, _) => {
            let live = source == "apex" || source == "frontier" || source.starts_with("frontier");
            if m.display_only && compact && !live {
                return None;
            }
            let text = if verdict == "SHADOW" {
                format!(
                    "FRT·{} SHADOW",
                    setup
                        .unwrap_or("TAPE")
                        .chars()
                        .take(4)
                        .collect::<String>()
                        .to_uppercase()
                )
            } else if live && verdict == "GO" {
                let prefix = if source.starts_with("frontier") || source == "frontier" {
                    format!(
                        "FRT·{}",
                        setup
                            .unwrap_or("TAPE")
                            .chars()
                            .take(4)
                            .collect::<String>()
                            .to_uppercase()
                    )
                } else if source == "apex" {
                    "APX".into()
                } else {
                    source.chars().take(3).collect::<String>().to_uppercase()
                };
                if short {
                    format!("{prefix} {conv:.2} GO SELL·SHORT")
                } else {
                    format!("{prefix} {conv:.2} GO")
                }
            } else if live && verdict == "VETO" {
                format!(
                    "{} VETO",
                    if source == "apex" { "APX" } else { "FRT" }
                )
            } else if source == "fsm" || verdict == "FSM" {
                // Honest label: FSM never places orders in apex_only.
                if compact {
                    "FSM".into()
                } else if m.display_only {
                    "FSM SKIP".into()
                } else {
                    "FSM".into()
                }
            } else if m.display_only {
                "SKIP".into()
            } else {
                "GO BUY".into()
            };
            let color = if verdict == "SHADOW" {
                "#BC8CFF"
            } else if m.display_only || verdict == "VETO" || verdict == "FSM" {
                "#6E7681"
            } else if short {
                "#F85149"
            } else {
                "#58A6FF"
            };
            let reason = match (live, verdict, setup) {
                (true, "GO", Some(s)) => Some(format!(
                    "{source}/{s} GO conv={conv:.2} — live path can place orders; check tape/spread"
                )),
                (true, "GO", None) => Some(format!(
                    "{source} GO conv={conv:.2} — live path can place orders"
                )),
                (true, "SHADOW", _) => Some(
                    "SHADOW — Frontier candidate (no order); manual OK for validation".into(),
                ),
                (true, "VETO", _) => Some(format!("{source} VETO — live path rejected")),
                _ => Some(
                    "FSM reference — does not place orders in apex_only; not a broker fill".into(),
                ),
            };
            Some(ChartMarker {
                time: m.bar_time,
                position: if short { "aboveBar" } else { "belowBar" },
                color,
                shape: if m.display_only || verdict == "FSM" || verdict == "VETO" {
                    "circle"
                } else if short {
                    "arrowDown"
                } else {
                    "arrowUp"
                },
                text,
                score: m.score,
                price: m.price,
                reason,
                size: if live && verdict == "GO" { 2 } else { 1 },
                is_real: false,
                display_only: m.display_only || verdict == "FSM" || verdict == "SHADOW",
            })
        }
        _ => None,
    }
}

/// Measured win-rate per playbook over its most recent closed trades
/// (all symbols — the badge answers "how reliable is this arrow TYPE",
/// not "how did this ticker do"). Returns `(win_rate_pct, n)` keyed by
/// playbook id (`"apex"`, `"fsm"`, ...). Capped at the last
/// `WR_WINDOW` closes per playbook so stale history ages out.
fn playbook_win_rates(
    entries: &[crate::state::JournalEntry],
) -> std::collections::HashMap<String, (f64, usize)> {
    const WR_WINDOW: usize = 30;
    let mut closes: std::collections::HashMap<String, Vec<bool>> =
        std::collections::HashMap::new();
    // Journal is append-ordered; iterate reversed so we keep the most
    // recent WR_WINDOW closes per playbook.
    for je in entries.iter().rev() {
        let Some(pnl) = je.pnl else { continue };
        let key = je.strategy_id.as_deref().unwrap_or("fsm").to_string();
        let v = closes.entry(key).or_default();
        if v.len() < WR_WINDOW {
            v.push(pnl > 0.0);
        }
    }
    closes
        .into_iter()
        .map(|(k, v)| {
            let n = v.len();
            let wins = v.iter().filter(|w| **w).count();
            (k, (wins as f64 / n.max(1) as f64 * 100.0, n))
        })
        .collect()
}

/// S34 v2 — Real-trade markers from the journal.
///
/// **One BUY at `entry_time`** and **one SELL at `exit_time`**
/// (when present) per `JournalEntry`. These always carry
/// `is_real=true`, `size=2` (bold), and **`★`-prefixed labels** to
/// make them visually unmistakable from FSM signals:
///
///   `★ BOT BUY @ $X.XX`
///   `★ BOT SELL @ $X.XX P/L $±N.NN`
///
/// Colors:
///   - LONG entry  → emerald `#10B981` (distinct from FSM bright cyan)
///   - SHORT entry → amber `#F59E0B` (rare)
///   - WIN exit    → emerald `#10B981`
///   - LOSS exit   → bright red `#DC2626`
///   - Neutral     → slate `#94A3B8`
fn journal_to_chart_markers(
    entries: &[crate::state::JournalEntry],
    symbol: &str,
) -> Vec<ChartMarker> {
    // Measured-confidence badge: computed across ALL symbols before
    // the per-symbol filter below.
    let win_rates = playbook_win_rates(entries);
    let mut out = Vec::with_capacity(entries.len() * 2);
    for je in entries.iter().filter(|je| je.symbol == symbol) {
        // BUY (entry) — always present.
        let (entry_color, entry_shape, entry_position) = match je.side {
            Direction::Long => ("#10B981", "arrowUp", "belowBar"),
            Direction::Short => ("#F59E0B", "arrowDown", "aboveBar"),
        };
        let strategy_suffix = match je.strategy_id.as_deref() {
            Some(sid) if !sid.is_empty() => format!(" [{}]", sid),
            _ => String::new(),
        };
        // PRIME F3 — fused conviction of the arbitration winner, when
        // the entry went through the PRIME engine. Rendered both in
        // the marker label (compact) and the tooltip reason (spelled
        // out) so the operator can see at a glance how confident the
        // engine was.
        let conviction_label = je
            .conviction
            .map(|c| format!(" ◆{:.0}%", c * 100.0))
            .unwrap_or_default();
        let conviction_reason = je
            .conviction
            .map(|c| format!(" — PRIME conviction {:.2}", c))
            .unwrap_or_default();
        // Measured-confidence badge: the playbook's real win rate over
        // its last ≤30 closes. On the label only with ≥5 samples (a
        // 1-trade "100%" would be noise dressed as evidence); always
        // spelled out in the tooltip.
        let playbook_key = je.strategy_id.as_deref().unwrap_or("fsm");
        let (wr_label, wr_reason) = match win_rates.get(playbook_key) {
            Some((wr, n)) if *n >= 5 => (
                format!(" WR{:.0}%", wr),
                format!(" — measured win rate {:.0}% over last {} closes", wr, n),
            ),
            Some((wr, n)) => (
                String::new(),
                format!(" — win rate {:.0}% but only {} closes (low evidence)", wr, n),
            ),
            None => (String::new(), " — no closed trades yet for this playbook".to_string()),
        };
        out.push(ChartMarker {
            time: je.entry_time,
            position: entry_position,
            color: entry_color,
            shape: entry_shape,
            text: format!(
                "★ BOT BUY @ ${:.2}{}{}",
                je.entry_price, conviction_label, wr_label
            ),
            score: je.entry_score,
            price: je.entry_price,
            reason: Some(format!(
                "REAL trade — bot opened {:?} {} @ ${:.4}{}{}{}",
                je.side, je.qty, je.entry_price, strategy_suffix, conviction_reason, wr_reason
            )),
            size: 2,
            is_real: true,
            display_only: false,
        });

        // SELL/EXIT (only present once the trade closes).
        if let (Some(exit_time), Some(exit_price)) = (je.exit_time, je.exit_price) {
            let pnl_str = je
                .pnl
                .map(|p| format!(" P/L ${:+.2}", p))
                .unwrap_or_default();
            let reason_str = je
                .exit_reason
                .as_deref()
                .map(|r| format!(" [{}]", r))
                .unwrap_or_default();
            let exit_color: &'static str = match je.pnl {
                Some(p) if p > 0.0 => "#10B981", // emerald win
                Some(p) if p < 0.0 => "#DC2626", // red loss
                _ => "#94A3B8",                  // neutral
            };
            let exit_position = match je.side {
                Direction::Long => "aboveBar",
                Direction::Short => "belowBar",
            };
            out.push(ChartMarker {
                time: exit_time,
                position: exit_position,
                color: exit_color,
                shape: "arrowDown",
                text: format!("★ BOT SELL @ ${:.2}{}", exit_price, pnl_str),
                score: 0.0,
                price: exit_price,
                reason: Some(format!(
                    "REAL exit — bot closed {} @ ${:.4}{}{}",
                    je.qty, exit_price, pnl_str, reason_str
                )),
                size: 2,
                is_real: true,
                display_only: false,
            });
        }
    }
    out
}

#[tauri::command]
pub fn get_markers(
    state: tauri::State<'_, Arc<TitanState>>,
    symbol: String,
    timeframe: String,
) -> Result<Vec<ChartMarker>, String> {
    let norm_tf = normalize_tf(&timeframe);
    // S35.6 — drop markers older than the per-TF max age. This
    // prevents ancient markers (e.g. Dec 2025 4h, Feb 2026 1h) from
    // dragging the chart price-axis to far-removed price levels and
    // creating the vertical "gap" Tony complained about.
    let now = chrono::Utc::now().timestamp();
    let max_age_secs = marker_max_age_secs(norm_tf);
    let cutoff = now.saturating_sub(max_age_secs);

    // APEX-only mode — FSM markers are reference-only; render compact
    // (small muted arrows, no labels) so real APEX trades own the chart.
    let compact_fsm = state
        .config
        .lock()
        .map(|c| c.evolution_protocol.apex_only)
        .unwrap_or(true);

    // Display floor (apex_only): weak FSM entry signals don't render at
    // all — an operator gauging the chart must not be tempted by
    // sub-threshold arrows. Exits stay (they pair with visible entries).
    const FSM_ENTRY_MIN_SCORE: f64 = 60.0;

    // FSM shadow markers (timeframe-scoped, attributed to engine eval).
    let fsm_markers: Vec<ChartMarker> = {
        let st = state.smarttrend.lock().map_err(|e| format!("lock: {e}"))?;
        st.markers
            .iter()
            .filter(|m| m.symbol == symbol && normalize_tf(&m.timeframe) == norm_tf)
            .filter(|m| m.bar_time >= cutoff)
            .filter(|m| {
                !(compact_fsm
                    && m.kind == MarkerKind::Entry
                    && m.score < FSM_ENTRY_MIN_SCORE)
            })
            .filter_map(|m| marker_to_chart(m, compact_fsm))
            .collect()
    };

    // Real-trade markers from the journal (timeframe-agnostic — a real
    // entry on a 5m bar must also show on the 1m chart).
    let real_markers: Vec<ChartMarker> = {
        let journal = state.journal.lock().map_err(|e| format!("lock: {e}"))?;
        journal_to_chart_markers(&journal.entries, &symbol)
    };

    let mut result = fsm_markers;
    result.extend(real_markers);
    result.sort_by_key(|m| m.time);
    Ok(result)
}
