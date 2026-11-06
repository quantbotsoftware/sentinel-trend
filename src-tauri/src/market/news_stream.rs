//! Real-time Alpaca news websocket (`wss://…/v1beta1/news`).
//!
//! The REST poller in `alpaca_news.rs` refreshes every 5 minutes —
//! fine as backfill, hours too slow for premarket catalysts. This
//! stream pushes Benzinga headlines within seconds of publication into
//! the same `NewsCache`, so every consumer (`news_recent` flag,
//! watchlist pills, PREMARKET_IGNITION catalyst gate) gets fresher
//! data with zero changes on their side.
//!
//! Auth + reconnect mirror `streamer.rs`: JSON auth message, then
//! subscribe `news:["*"]`; exponential backoff capped at 60s.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use super::alpaca_news::{global_cache, NewsItem};
use crate::state::TitanState;

const NEWS_WS_URL: &str = "wss://stream.data.alpaca.markets/v1beta1/news";
const LOOP_NAME: &str = "news_stream";

/// Fan-out of real-time headlines to in-process consumers (MERIDIAN C-PM
/// `premarket_live`). Lagging receivers drop the oldest items — headline
/// bursts are small and consumers are fast, so 1024 is generous.
fn headline_bus() -> &'static tokio::sync::broadcast::Sender<NewsItem> {
    static TX: std::sync::OnceLock<tokio::sync::broadcast::Sender<NewsItem>> = std::sync::OnceLock::new();
    TX.get_or_init(|| tokio::sync::broadcast::channel(1024).0)
}

/// Subscribe to real-time headlines as they arrive on the websocket.
pub fn subscribe_headlines() -> tokio::sync::broadcast::Receiver<NewsItem> {
    headline_bus().subscribe()
}

/// Publish a headline to in-process subscribers (also used by tests).
pub fn publish_headline(item: NewsItem) {
    let _ = headline_bus().send(item);
}

/// Parse one websocket frame into news items. Frames are JSON arrays
/// of tagged messages; only `T == "n"` entries carry a news payload.
/// Pure — unit-testable without a socket.
pub fn parse_news_frame(text: &str) -> Vec<NewsItem> {
    let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        return Vec::new();
    };
    values
        .into_iter()
        .filter(|v| v.get("T").and_then(|t| t.as_str()) == Some("n"))
        .filter_map(|v| serde_json::from_value::<NewsItem>(v).ok())
        .collect()
}

fn is_auth_ok(text: &str) -> bool {
    serde_json::from_str::<Vec<serde_json::Value>>(text)
        .map(|vals| {
            vals.iter().any(|v| {
                v.get("T").and_then(|t| t.as_str()) == Some("success")
                    && v.get("msg").and_then(|m| m.as_str()) == Some("authenticated")
            })
        })
        .unwrap_or(false)
}

pub async fn run_news_stream(state: Arc<TitanState>) {
    tracing::info!(component = LOOP_NAME, "news stream task started");
    let mut backoff_secs: u64 = 1;

    loop {
        if state.shutdown.is_cancelled() {
            break;
        }
        let (key, secret) = {
            let cfg = state.config.lock().unwrap_or_else(|p| p.into_inner());
            (
                cfg.market.alpaca_key_id.clone(),
                cfg.market.alpaca_secret.clone(),
            )
        };
        if key.is_empty() || secret.is_empty() {
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }

        match connect_and_pump(&state, &key, &secret).await {
            Ok(()) => {
                // Clean disconnect (shutdown) — loop re-checks the token.
                backoff_secs = 1;
            }
            Err(e) => {
                tracing::warn!(
                    component = LOOP_NAME,
                    error = %e,
                    backoff_secs,
                    "news stream disconnected; reconnecting"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(60);
            }
        }
    }
    tracing::info!(component = LOOP_NAME, "news stream task exiting");
}

async fn connect_and_pump(
    state: &TitanState,
    key: &str,
    secret: &str,
) -> Result<(), String> {
    let (ws, _) = connect_async(NEWS_WS_URL)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (mut tx, mut rx) = ws.split();

    let auth = serde_json::json!({"action": "auth", "key": key, "secret": secret});
    tx.send(Message::Text(auth.to_string()))
        .await
        .map_err(|e| format!("auth send: {e}"))?;

    // Wait for authenticated (bounded).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = tokio::time::timeout_at(deadline, rx.next())
            .await
            .map_err(|_| "auth timeout".to_string())?
            .ok_or("closed during auth")?
            .map_err(|e| format!("auth recv: {e}"))?;
        if let Message::Text(t) = msg {
            if is_auth_ok(&t) {
                break;
            }
            if t.contains("auth failed") || t.contains("\"error\"") {
                return Err(format!("auth rejected: {t}"));
            }
        }
    }

    let sub = serde_json::json!({"action": "subscribe", "news": ["*"]});
    tx.send(Message::Text(sub.to_string()))
        .await
        .map_err(|e| format!("subscribe send: {e}"))?;
    tracing::info!(component = LOOP_NAME, "news stream authenticated + subscribed (*)");

    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return Ok(()),
            msg = rx.next() => {
                let msg = msg.ok_or("stream closed")?.map_err(|e| format!("recv: {e}"))?;
                match msg {
                    Message::Text(text) => {
                        for item in parse_news_frame(&text) {
                            tracing::info!(
                                component = LOOP_NAME,
                                symbols = ?item.symbols,
                                source = %item.source,
                                headline = %item.headline,
                                "real-time headline"
                            );
                            for sym in item.symbols.clone() {
                                global_cache().push_item(&sym, item.clone()).await;
                            }
                            publish_headline(item);
                        }
                    }
                    Message::Ping(p) => {
                        let _ = tx.send(Message::Pong(p)).await;
                    }
                    Message::Close(_) => return Err("server closed".into()),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_news_frame_and_ignores_control_messages() {
        let frame = r#"[
            {"T":"success","msg":"authenticated"},
            {"T":"n","id":42,"headline":"ACME wins FDA approval",
             "created_at":"2026-08-11T11:30:00Z","symbols":["ACME","ACMW"],
             "source":"benzinga","url":"https://example.com/a"}
        ]"#;
        let items = parse_news_frame(frame);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, 42);
        assert_eq!(items[0].symbols, vec!["ACME", "ACMW"]);
    }

    #[test]
    fn malformed_frames_yield_empty() {
        assert!(parse_news_frame("not json").is_empty());
        assert!(parse_news_frame("{\"T\":\"n\"}").is_empty()); // not an array
    }

    #[test]
    fn auth_ok_detection() {
        assert!(is_auth_ok(r#"[{"T":"success","msg":"authenticated"}]"#));
        assert!(!is_auth_ok(r#"[{"T":"success","msg":"connected"}]"#));
        assert!(!is_auth_ok("garbage"));
    }
}
