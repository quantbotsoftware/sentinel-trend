use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::market::alpaca_news::build_news_map;
use crate::market::catalyst::{is_halted_combined, CatalystFlags, CatalystInput, CatalystWeights};
use crate::market::market_state::{HaltState, MarketDataState, WatchlistMetrics};
use crate::state::{FsmPhase, Mover, TitanState};

/// Per-symbol view served to the watchlist UI.
///
/// S13 added `catalyst_flags` / `catalyst_pills`. S14 extended again
/// with the real-time S11 surface (`bid`, `ask`, `spread_bps`,
/// `last_trade_age_seconds`, `is_stale`) so the UI can render the same
/// columns as the scanner table without an extra round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct WatchlistItem {
    pub symbol: String,
    pub has_earnings: bool,
    pub st_score: f64,
    pub st_state: String,
    pub price: f64,
    pub change_percent: f64,
    pub volume: f64,
    pub rvol: f64,
    pub gap_percent: f64,
    /// S14: NBBO bid (0.0 when no fresh quote available).
    pub bid: f64,
    /// S14: NBBO ask (0.0 when no fresh quote available).
    pub ask: f64,
    /// S14: NBBO mid spread in basis points.
    pub spread_bps: f64,
    /// S14: seconds since the last consolidated trade.
    pub last_trade_age_seconds: u32,
    /// S14: true when the trade is older than the RTH stale threshold.
    pub is_stale: bool,
    /// S13: catalyst engine output (earn/news/halt/gap_dir/vol_spike + score).
    pub catalyst_flags: CatalystFlags,
    /// S13: pre-computed pill labels for the UI (S14 wires the column).
    pub catalyst_pills: Vec<String>,
}

#[tauri::command]
pub fn get_movers(state: tauri::State<'_, Arc<TitanState>>) -> Vec<Mover> {
    use crate::market::scanner::{now_et, session_date_et_str};
    let today = session_date_et_str(now_et());
    let mut market = state.market.lock().unwrap();
    // SC11: never hand the UI yesterday's movers.
    if market.movers.iter().any(|m| m.session_date_et != today) {
        market.movers.retain(|m| m.session_date_et == today);
    }
    market.movers.clone()
}

/// Per-symbol intermediate computed entirely under Tier-A.
struct WatchlistTierA {
    symbol: String,
    has_earnings: bool,
    st_score: f64,
    st_state: String,
    /// Legacy `scan_results` snapshot (only used in the fallback branch
    /// when the symbol is absent from `watchlist_metrics`).
    scan_fallback: Option<ScanFallback>,
    catalyst_weights: CatalystWeights,
}

#[derive(Clone, Copy)]
struct ScanFallback {
    change_percent: f64,
    volume: f64,
    rvol: f64,
    gap_percent: f64,
    last_trade_age_seconds: u32,
}

/// `get_watchlist` — primary source is `MarketDataState.watchlist_metrics`
/// (Tier B6, refreshed every 30 s by `watchlist_refresh`). When a symbol
/// isn't yet in B6 (cold-start window, freshly added favorite), the
/// command falls back to the older path: `scan_results` + `last_prices`
/// + `halts` + inline catalyst computation.
///
/// Lock ordering proof:
///
///   Tier-A : config (A1) → market (A2) → smarttrend (A3)  → drop
///   Tier-B : last_prices (B1) → halts (B5) → watchlist_metrics (B6)
///   News   : own mutex (neither tier)
///   Emit   : N/A — this is a synchronous query, no Tauri event.
///
/// Tier-A and Tier-B are NEVER held at the same time. Each Tier-B
/// helper is clone-and-drop.
#[tauri::command]
pub async fn get_watchlist(
    state: tauri::State<'_, Arc<TitanState>>,
    market_data: tauri::State<'_, Arc<MarketDataState>>,
) -> Result<Vec<WatchlistItem>, String> {
    // ─── Tier-A pass ────────────────────────────────────────────────
    let tier_a: Vec<WatchlistTierA> = {
        let cfg = state.config.lock().map_err(|e| format!("config lock: {e}"))?;
        let au_enabled = cfg.auto_universe.enabled;
        let favorites = cfg.ui.favorites.clone();
        let catalyst_weights = cfg.catalyst.weights;
        drop(cfg);

        let market = state.market.lock().map_err(|e| format!("market lock: {e}"))?;
        let earnings = market.earnings_today.clone();
        // S18: when AutoUniverse is enabled, the watchlist mirrors
        // pinned ∪ auto_pool so the UI sees auto-injected symbols
        // (MASK/TRAW/...) alongside the sacred pinned slots. Falls
        // back to `cfg.ui.favorites` when S18 is off — preserves
        // legacy behavior bit-for-bit.
        //
        // Order: pinned first (preserving user-curated order), then
        // auto_pool sorted by score desc (best opportunity on top).
        // `active_set()` is intentionally not used because HashSet
        // iteration is unordered and would shuffle rows on every
        // re-render.
        let watched: Vec<String> = if au_enabled {
            let au = &market.auto_universe;
            let mut v: Vec<String> = au.pinned.to_vec();
            let mut autos: Vec<(String, f64)> = au
                .auto_pool
                .iter()
                .map(|(k, e)| (k.clone(), e.last_score))
                .collect();
            autos.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            v.extend(autos.into_iter().map(|(s, _)| s));
            v
        } else {
            favorites.clone()
        };
        let scan_fallback: HashMap<String, ScanFallback> = market
            .scan_results
            .iter()
            .map(|r| {
                (
                    r.symbol.clone(),
                    ScanFallback {
                        change_percent: r.change_percent,
                        volume: r.volume,
                        rvol: r.rvol,
                        gap_percent: r.gap_percent,
                        last_trade_age_seconds: r.last_trade_age_seconds,
                    },
                )
            })
            .collect();
        drop(market);

        let st = state.smarttrend.lock().map_err(|e| format!("smarttrend lock: {e}"))?;
        let mut out = Vec::with_capacity(watched.len());
        for sym in &watched {
            let has_earnings = earnings.contains(sym);
            let (st_score, st_state) = st
                .fsm
                .get(sym)
                .map_or((0.0, "Idle".to_string()), |fsm| {
                    let state_str = match fsm.phase {
                        FsmPhase::Idle | FsmPhase::Stalking => "Idle",
                        FsmPhase::Confirming => "Confirming",
                        FsmPhase::Active => "Active",
                        FsmPhase::Pullback => "Pullback",
                        FsmPhase::Exit => "Exit",
                    };
                    (fsm.score, state_str.to_string())
                });
            out.push(WatchlistTierA {
                symbol: sym.clone(),
                has_earnings,
                st_score,
                st_state,
                scan_fallback: scan_fallback.get(sym).copied(),
                catalyst_weights,
            });
        }
        drop(st);
        out
    };

    // ─── Tier-B pass (ascending: B1 → B5 → B6) ──────────────────────
    let (price_map, halts, metrics_map) = {
        let last_prices = market_data
            .last_prices
            .lock()
            .map_err(|e| format!("last_prices lock: {e}"))?;
        let prices: HashMap<String, f64> = tier_a
            .iter()
            .map(|t| (t.symbol.clone(), last_prices.get(&t.symbol).copied().unwrap_or(0.0)))
            .collect();
        drop(last_prices);
        let halts = market_data.halts_snapshot();
        let metrics = market_data.watchlist_metrics_snapshot();
        (prices, halts, metrics)
    };

    // ─── News cache (own mutex) ─────────────────────────────────────
    let symbols: Vec<String> = tier_a.iter().map(|t| t.symbol.clone()).collect();
    let news_map = build_news_map(&symbols).await;

    let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);

    Ok(tier_a
        .into_iter()
        .map(|t| build_item(t, &price_map, &halts, &metrics_map, &news_map, now_et))
        .collect())
}

/// Per-symbol assembly. Pure with respect to locks (none held).
///
/// Re-computes `catalyst_flags` so the UI sees the correct `has_earnings`
/// — `watchlist_refresh` writes B6 with `has_earnings=false` because the
/// earnings calendar lives in Tier-A (out of reach of the refresh task).
fn build_item(
    t: WatchlistTierA,
    price_map: &HashMap<String, f64>,
    halts: &HashMap<String, HaltState>,
    metrics_map: &HashMap<String, WatchlistMetrics>,
    news_map: &HashMap<String, Vec<crate::market::alpaca_news::NewsItem>>,
    now_et: chrono::DateTime<chrono_tz::Tz>,
) -> WatchlistItem {
    let key = t.symbol.to_uppercase();
    let has_recent_news = news_map.contains_key(&key);

    // PRIMARY: fresh metrics from the 30s watchlist refresh loop.
    if let Some(m) = metrics_map.get(&key).or_else(|| metrics_map.get(&t.symbol)) {
        let catalyst_flags = CatalystFlags::compute(
            &CatalystInput {
                symbol: &t.symbol,
                change_pct: m.change_percent,
                gap_pct: m.gap_percent,
                rvol: m.rvol,
                has_earnings: t.has_earnings,
                has_recent_news,
                is_halted: m.catalyst_flags.halted,
                has_offering: crate::market::edgar::has_recent_offering(&t.symbol),
            },
            &t.catalyst_weights,
        );
        let catalyst_pills = catalyst_flags.pills_owned();
        return WatchlistItem {
            symbol: t.symbol,
            has_earnings: t.has_earnings,
            st_score: t.st_score,
            st_state: t.st_state,
            price: m.price,
            change_percent: m.change_percent,
            volume: m.volume,
            rvol: m.rvol,
            gap_percent: m.gap_percent,
            bid: m.bid,
            ask: m.ask,
            spread_bps: m.spread_bps,
            last_trade_age_seconds: m.last_trade_age_seconds,
            is_stale: m.is_stale,
            catalyst_flags,
            catalyst_pills,
        };
    }

    // FALLBACK: scan_results + last_prices + halts (cold-start window).
    let price = price_map.get(&t.symbol).copied().unwrap_or(0.0);
    let fb = t.scan_fallback.unwrap_or(ScanFallback {
        change_percent: 0.0,
        volume: 0.0,
        rvol: 0.0,
        gap_percent: 0.0,
        last_trade_age_seconds: 0,
    });
    let is_halted = is_halted_combined(halts, &key, fb.last_trade_age_seconds, now_et);
    let catalyst_flags = CatalystFlags::compute(
        &CatalystInput {
            symbol: &t.symbol,
            change_pct: fb.change_percent,
            gap_pct: fb.gap_percent,
            rvol: fb.rvol,
            has_earnings: t.has_earnings,
            has_recent_news,
            is_halted,
            has_offering: crate::market::edgar::has_recent_offering(&t.symbol),
        },
        &t.catalyst_weights,
    );
    let catalyst_pills = catalyst_flags.pills_owned();
    WatchlistItem {
        symbol: t.symbol,
        has_earnings: t.has_earnings,
        st_score: t.st_score,
        st_state: t.st_state,
        price,
        change_percent: fb.change_percent,
        volume: fb.volume,
        rvol: fb.rvol,
        gap_percent: fb.gap_percent,
        bid: 0.0,
        ask: 0.0,
        spread_bps: 0.0,
        last_trade_age_seconds: fb.last_trade_age_seconds,
        is_stale: false,
        catalyst_flags,
        catalyst_pills,
    }
}

/// v2.4.3 — UI helper for the `NEWS` catalyst pill. The watchlist
/// row only renders a boolean flag; this command returns the actual
/// headlines for a click-through modal. Reads straight from the
/// process-wide `NewsCache` (populated every 5 min by
/// `run_catalyst_refresh_loop`) so it's effectively free — no
/// outbound Alpaca call here. If the symbol isn't in cache or the
/// entry has TTL-expired, returns an empty list (UI shows "no
/// recent news cached, refresh due in <N>s").
#[derive(serde::Serialize)]
pub struct NewsHeadline {
    pub id: i64,
    pub headline: String,
    pub author: String,
    pub source: String,
    pub created_at: String,
    pub symbols: Vec<String>,
    pub url: String,
}

#[tauri::command]
pub async fn get_news_for_symbol(symbol: String) -> Result<Vec<NewsHeadline>, String> {
    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() || symbol.len() > 10 {
        return Err("Invalid symbol".to_string());
    }
    let items = crate::market::alpaca_news::global_cache()
        .get(&symbol)
        .await
        .unwrap_or_default();
    Ok(items
        .into_iter()
        .map(|n| NewsHeadline {
            id: n.id,
            headline: n.headline,
            author: n.author,
            source: n.source,
            created_at: n.created_at,
            symbols: n.symbols,
            url: n.url,
        })
        .collect())
}

/// v2.4.3 — launch a URL in the user's default browser, outside the
/// Tauri webview. Sandbox-friendly alternative to enabling the
/// `tauri.allowlist.shell.open` feature (which would require a CSP
/// loosening and a full rebuild of the bundle).
///
/// Validation: only `http(s)://` schemes accepted. No shell expansion
/// (we pass the URL as a single argv to `xdg-open`, not via shell).
/// Errors propagate to the frontend so it can show a toast.
#[tauri::command]
pub fn open_external_url(url: String) -> Result<(), String> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!(
            "Refusing to open non-http(s) URL: {}",
            url.chars().take(40).collect::<String>()
        ));
    }
    // Linux desktop: `xdg-open` hands the URL to the system's default
    // browser. macOS would use `open`, Windows `cmd /C start`; add
    // when needed — Tony is on linux 6.18.
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("xdg-open failed: {e}"))?;
    Ok(())
}

#[tauri::command]
pub fn add_to_watchlist(
    state: tauri::State<'_, Arc<TitanState>>,
    symbol: String,
) -> Result<String, String> {
    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty() || symbol.len() > 10 {
        return Err("Invalid symbol".to_string());
    }

    {
        let mut cfg = state.config.lock().unwrap();
        if cfg.ui.favorites.contains(&symbol) {
            return Ok(format!("{symbol} already in watchlist"));
        }
        if cfg.ui.favorites.len() >= 20 {
            return Err("Watchlist max 20 symbols".to_string());
        }
        cfg.ui.favorites.push(symbol.clone());
    }

    if let Err(e) = crate::ops::persistence::save_config(&state) {
        tracing::error!("Failed to save config after watchlist add: {e}");
    }

    tracing::info!("Watchlist: added {symbol}");
    Ok(format!("{symbol} added to watchlist"))
}

#[tauri::command]
pub fn remove_from_watchlist(
    state: tauri::State<'_, Arc<TitanState>>,
    symbol: String,
) -> Result<String, String> {
    let symbol = symbol.trim().to_uppercase();
    if crate::market::auto_universe::AutoUniverse::is_permanent_chart_symbol(&symbol) {
        return Err(format!(
            "{symbol} is a permanent chart symbol and cannot be removed from the watchlist"
        ));
    }

    let removed = {
        let mut cfg = state.config.lock().unwrap();
        let before = cfg.ui.favorites.len();
        cfg.ui.favorites.retain(|s| s != &symbol);
        cfg.ui.favorites.len() < before
    };

    if !removed {
        return Ok(format!("{symbol} not in watchlist"));
    }

    if let Err(e) = crate::ops::persistence::save_config(&state) {
        tracing::error!("Failed to save config after watchlist remove: {e}");
    }

    tracing::info!("Watchlist: removed {symbol}");
    Ok(format!("{symbol} removed from watchlist"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rth_now() -> chrono::DateTime<chrono_tz::Tz> {
        chrono::TimeZone::with_ymd_and_hms(&chrono_tz::US::Eastern, 2026, 4, 29, 10, 30, 0)
            .unwrap()
    }

    fn b6_metrics(symbol: &str) -> WatchlistMetrics {
        WatchlistMetrics {
            symbol: symbol.to_string(),
            price: 200.0,
            bid: 199.95,
            ask: 200.05,
            spread_bps: 5.0,
            change_percent: 3.0,
            volume: 4_000_000.0,
            rvol: 4.5,
            gap_percent: 2.0,
            last_trade_age_seconds: 1,
            is_stale: false,
            catalyst_flags: CatalystFlags::default(),
            catalyst_pills: Vec::new(),
            news_sentiment: None,
            updated_at: Utc::now(),
        }
    }

    fn tier_a(symbol: &str, has_earnings: bool) -> WatchlistTierA {
        WatchlistTierA {
            symbol: symbol.to_string(),
            has_earnings,
            st_score: 50.0,
            st_state: "Confirming".to_string(),
            scan_fallback: None,
            catalyst_weights: CatalystWeights::default(),
        }
    }

    #[test]
    fn watchlist_item_extended_serialize() {
        let item = WatchlistItem {
            symbol: "AAPL".to_string(),
            has_earnings: true,
            st_score: 62.5,
            st_state: "Stalking".to_string(),
            price: 150.0,
            change_percent: 2.5,
            volume: 1_000_000.0,
            rvol: 3.2,
            gap_percent: 1.5,
            bid: 149.95,
            ask: 150.05,
            spread_bps: 6.67,
            last_trade_age_seconds: 4,
            is_stale: false,
            catalyst_flags: CatalystFlags::default(),
            catalyst_pills: Vec::new(),
        };
        let json = serde_json::to_string(&item).unwrap();
        for needle in [
            "AAPL", "Stalking", "\"price\":150",
            "\"bid\":149.95", "\"ask\":150.05",
            "\"spread_bps\":6.67", "\"last_trade_age_seconds\":4",
            "\"is_stale\":false", "\"catalyst_flags\"", "\"catalyst_pills\"",
        ] {
            assert!(json.contains(needle), "missing field {needle} in {json}");
        }
    }

    #[test]
    fn build_item_prefers_metrics_over_scan_fallback() {
        let mut metrics = HashMap::new();
        metrics.insert("AAPL".into(), b6_metrics("AAPL"));
        let prices: HashMap<String, f64> = HashMap::new(); // empty — proves we use B6
        let halts: HashMap<String, HaltState> = HashMap::new();
        let news: HashMap<String, Vec<_>> = HashMap::new();

        let mut t = tier_a("AAPL", false);
        t.scan_fallback = Some(ScanFallback {
            change_percent: 99.0, // wrong on purpose — must NOT surface
            volume: 1.0,
            rvol: 1.0,
            gap_percent: 1.0,
            last_trade_age_seconds: 99,
        });

        let item = build_item(t, &prices, &halts, &metrics, &news, rth_now());
        assert!((item.price - 200.0).abs() < 1e-9, "B6 price wins");
        assert!((item.change_percent - 3.0).abs() < 1e-9, "B6 change wins");
        assert!((item.bid - 199.95).abs() < 1e-9);
        assert!((item.ask - 200.05).abs() < 1e-9);
        assert_eq!(item.last_trade_age_seconds, 1);
        assert!(!item.is_stale);
    }

    #[test]
    fn build_item_falls_back_to_scan_when_metrics_missing() {
        let metrics: HashMap<String, WatchlistMetrics> = HashMap::new();
        let mut prices = HashMap::new();
        prices.insert("MSFT".to_string(), 333.0);
        let halts: HashMap<String, HaltState> = HashMap::new();
        let news: HashMap<String, Vec<_>> = HashMap::new();

        let mut t = tier_a("MSFT", false);
        t.scan_fallback = Some(ScanFallback {
            change_percent: 1.5,
            volume: 800_000.0,
            rvol: 2.0,
            gap_percent: 0.5,
            last_trade_age_seconds: 7,
        });

        let item = build_item(t, &prices, &halts, &metrics, &news, rth_now());
        assert!((item.price - 333.0).abs() < 1e-9);
        assert!((item.change_percent - 1.5).abs() < 1e-9);
        assert!((item.bid - 0.0).abs() < f64::EPSILON, "fallback has no bid");
        assert!((item.ask - 0.0).abs() < f64::EPSILON, "fallback has no ask");
        assert!(!item.is_stale);
    }

    #[test]
    fn build_item_recomputes_catalyst_with_earnings() {
        // B6 metrics carry has_earnings=false; build_item must override
        // with the Tier-A earnings flag so EARN pills appear correctly.
        let mut metrics = HashMap::new();
        metrics.insert("EARN".into(), b6_metrics("EARN"));
        let prices: HashMap<String, f64> = HashMap::new();
        let halts: HashMap<String, HaltState> = HashMap::new();
        let news: HashMap<String, Vec<_>> = HashMap::new();

        let t = tier_a("EARN", true /* has_earnings */);
        let item = build_item(t, &prices, &halts, &metrics, &news, rth_now());
        assert!(item.catalyst_flags.earnings, "earnings flag must be re-applied from Tier-A");
        assert!(item.catalyst_pills.contains(&"EARN".to_string()));
    }
}
