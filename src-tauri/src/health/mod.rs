//! Health-pulse module — runtime self-observability for the bot.
//!
//! Exposes a single Tauri command (`get_health_snapshot`) that
//! aggregates uptime, last-trade timestamp, error rate, WebSocket
//! status, and broker connectivity into a JSON payload. The watchdog
//! script in `monitoring/watchdog.sh` polls this every 5 min and
//! triggers `notify-send` on the desktop if any field crosses a
//! critical threshold.
//!
//! Design choices:
//!   - Single function, no background task required.
//!   - Reads only Tier-A locks (config + system + trading + risk),
//!     never blocking on Tier-B (market data) so the health probe
//!     stays cheap even when streamer is hot.
//!   - Pure JSON output — no struct dependency on the watchdog side.

pub mod health_check;
