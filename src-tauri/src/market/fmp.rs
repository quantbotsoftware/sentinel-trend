use std::collections::HashSet;
use std::sync::Arc;

use chrono::Timelike;
use serde::Deserialize;
use tauri::AppHandle;

use crate::state::TitanState;

/// Compute today/tomorrow date strings anchored in Eastern Time.
/// Ensures rollover happens at midnight ET, not UTC.
pub fn et_today_tomorrow(now: chrono::DateTime<chrono::Utc>) -> (String, String) {
    let now_et = now.with_timezone(&chrono_tz::US::Eastern);
    let today = now_et.format("%Y-%m-%d").to_string();
    let tomorrow = (now_et + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    (today, tomorrow)
}

/// Replace occurrences of `key` in `url` with `***` for safe logging.
#[allow(dead_code)]
pub fn redact_url(url: &str, key: &str) -> String {
    if key.is_empty() {
        url.to_string()
    } else {
        url.replace(key, "***")
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct FmpEarningsItem {
    symbol: Option<String>,
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FmpActiveItem {
    symbol: Option<String>,
    #[serde(alias = "changesPercentage")]
    changes_pct: Option<f64>,
    price: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FmpMoverProposal {
    pub symbol: String,
    pub price: f64,
    pub change_pct: f64,
}

pub async fn fetch_earnings_today(
    client: &reqwest::Client,
    api_key: &str,
) -> HashSet<String> {
    if api_key.is_empty() {
        return HashSet::new();
    }

    let (today, tomorrow) = et_today_tomorrow(chrono::Utc::now());

    // S17.6 — FMP migrated v3 to /stable in Aug 2025. The legacy URL
    // (`/api/v3/earning_calendar`, singular) now returns:
    //   `{"Error Message":"Legacy Endpoint : Due to Legacy endpoints
    //    being no longer supported - This endpoint is only available
    //    for legacy users who have valid subscriptions prior August
    //    31, 2025."}`
    // surfaced to logs as "FMP earnings API error: 403 Forbidden".
    // Response shape (`symbol`, `date`, `epsActual`, `epsEstimated`,
    // `revenueActual`, `revenueEstimated`, `lastUpdated`) is a superset
    // of the v3 payload so `FmpEarningsItem` deserialisation still
    // works unchanged.
    //
    // SECURITY: FMP requires apikey in query string. Never log this URL directly.
    let url = format!(
        "https://financialmodelingprep.com/stable/earnings-calendar?from={today}&to={tomorrow}&apikey={api_key}"
    );

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<Vec<FmpEarningsItem>>().await {
                Ok(items) => {
                    let symbols: HashSet<String> = items
                        .into_iter()
                        .filter_map(|e| e.symbol)
                        .filter(|s| !s.is_empty())
                        .collect();
                    tracing::info!("FMP: {} symbols with earnings today/tomorrow", symbols.len());
                    symbols
                }
                Err(e) => {
                    tracing::warn!("FMP earnings parse error: {e}");
                    HashSet::new()
                }
            }
        }
        Ok(resp) => {
            tracing::warn!("FMP earnings API error: {}", resp.status());
            HashSet::new()
        }
        Err(e) => {
            tracing::warn!("FMP earnings request failed: {e}");
            HashSet::new()
        }
    }
}

pub async fn fetch_premarket_movers(
    client: &reqwest::Client,
    api_key: &str,
) -> Vec<FmpMoverProposal> {
    if api_key.is_empty() {
        return Vec::new();
    }

    // S17.6 — same FMP v3 → /stable migration as `fetch_earnings_today`.
    // Response shape (`symbol`, `price`, `changesPercentage`, `name`,
    // `change`, `exchange`) keeps the fields `FmpActiveItem` already
    // deserialises (with the `changesPercentage` serde alias).
    //
    // SECURITY: FMP requires apikey in query string. Never log this URL directly.
    let url = format!(
        "https://financialmodelingprep.com/stable/most-actives?apikey={api_key}"
    );

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<Vec<FmpActiveItem>>().await {
                Ok(items) => {
                    let movers: Vec<FmpMoverProposal> = items
                        .into_iter()
                        .filter_map(|a| {
                            let sym = a.symbol?;
                            if sym.is_empty() { return None; }
                            Some(FmpMoverProposal {
                                symbol: sym,
                                price: a.price.unwrap_or(0.0),
                                change_pct: a.changes_pct.unwrap_or(0.0),
                            })
                        })
                        .take(5)
                        .collect();
                    tracing::info!("FMP: {} premarket movers fetched", movers.len());
                    movers
                }
                Err(e) => {
                    tracing::warn!("FMP actives parse error: {e}");
                    Vec::new()
                }
            }
        }
        Ok(resp) => {
            tracing::warn!("FMP actives API error: {}", resp.status());
            Vec::new()
        }
        Err(e) => {
            tracing::warn!("FMP actives request failed: {e}");
            Vec::new()
        }
    }
}

/// Called once at startup + once at ~09:00 ET daily.
pub async fn run_fmp_schedule(
    state: Arc<TitanState>,
    app: AppHandle,
) {
    let fmp_key = {
        let cfg = state.config.lock().unwrap();
        cfg.market.fmp_api_key.clone()
    };

    if fmp_key.is_empty() {
        tracing::info!("FMP: no API key configured, skipping");
        return;
    }

    let earnings = fetch_earnings_today(&state.http_client, &fmp_key).await;
    if !earnings.is_empty() {
        let mut market = state.market.lock().unwrap();
        market.earnings_today = earnings;
    }

    loop {
        if state.shutdown.is_cancelled() {
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
            _ = state.shutdown.cancelled() => { break; }
        }

        let now_et = chrono::Utc::now().with_timezone(&chrono_tz::US::Eastern);
        let et_hour = now_et.hour();
        let et_min = now_et.minute();

        if et_hour == 9 && et_min < 5 {
            let movers = fetch_premarket_movers(&state.http_client, &fmp_key).await;
            for m in &movers {
                let _ = tauri::Manager::emit_all(
                    &app,
                    "fmp-mover-propose",
                    m,
                );
            }
            tracing::info!("FMP: proposed {} premarket movers via toast", movers.len());

            let earnings = fetch_earnings_today(&state.http_client, &fmp_key).await;
            if !earnings.is_empty() {
                let mut market = state.market.lock().unwrap();
                market.earnings_today = earnings;
            }

            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
                _ = state.shutdown.cancelled() => { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmp_mover_proposal_serialize() {
        let p = FmpMoverProposal {
            symbol: "NVDA".to_string(),
            price: 800.0,
            change_pct: 3.2,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("NVDA"));
        assert!(json.contains("3.2"));
    }

    #[test]
    fn redact_url_replaces_key() {
        let url = "https://example.com/api?apikey=SECRET123&symbol=AAPL";
        assert_eq!(
            redact_url(url, "SECRET123"),
            "https://example.com/api?apikey=***&symbol=AAPL"
        );
    }

    #[test]
    fn redact_url_empty_key_returns_original() {
        let url = "https://example.com/api?symbol=AAPL";
        assert_eq!(redact_url(url, ""), url);
    }

    #[test]
    fn fmp_date_format_uses_eastern_winter() {
        // 2026-12-16 04:30 UTC = 2026-12-15 23:30 ET (EST, UTC-5)
        let utc = chrono::NaiveDate::from_ymd_opt(2026, 12, 16)
            .unwrap()
            .and_hms_opt(4, 30, 0)
            .unwrap()
            .and_utc();
        let (today, _) = et_today_tomorrow(utc);
        assert_eq!(today, "2026-12-15");
    }

    #[test]
    fn fmp_date_format_uses_eastern_summer() {
        // 2026-06-16 02:30 UTC = 2026-06-15 22:30 ET (EDT, UTC-4)
        let utc = chrono::NaiveDate::from_ymd_opt(2026, 6, 16)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap()
            .and_utc();
        let (today, _) = et_today_tomorrow(utc);
        assert_eq!(today, "2026-06-15");
    }

    #[test]
    fn empty_key_returns_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = reqwest::Client::new();
            let result = fetch_earnings_today(&client, "").await;
            assert!(result.is_empty());
            let movers = fetch_premarket_movers(&client, "").await;
            assert!(movers.is_empty());
        });
    }
}
