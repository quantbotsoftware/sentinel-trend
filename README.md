# SentinelTrend — Algorithmic Trading Desktop App (Rust + Tauri + Alpaca)

A production-grade desktop trading application for US equities, built on
**Tauri 2 (Rust backend + JavaScript frontend)** with full
**Alpaca** brokerage integration. ~67,000 lines of code.

> **What this is:** a well-architected, batteries-included **trading engine
> and infrastructure**. It ships with example strategies, but its value is the
> engine — robust order execution, risk controls, live market data, scanning,
> and charting. **Bring your own strategy.** Released **100% FREE & OPEN-SOURCE (MIT)** strictly **AS-IS**, without warranty, without liability, and without technical support.

---

## Highlights

- **Full Alpaca integration** — REST + live WebSocket (minute bars, quotes,
  trade-update stream). Paper and live modes.
- **Robust order execution** — bracket orders (entry + stop-loss + take-profit),
  deterministic client-order-id idempotency, retry on 5xx, partial-fill
  handling, broker-side position reconciliation.
- **Risk engine** — tiered circuit breakers (caution / halt / panic), daily
  P&L reset on the ET trading day, equity sync, per-trade loss caps,
  liquidate-all and emergency-flat hotkeys.
- **Cash/wallet bookkeeping** — internal ledger reconciled to broker cash via
  startup sync + periodic heartbeat.
- **Market scanner** — full US equity universe, configurable prefilter
  (price / volume / RVOL / gap / change), composite scoring, catalyst/news
  pipeline, auto-promotion of movers to a live watchlist.
- **Real-time charting** — TradingView Lightweight-Charts, multi-timeframe
  (1m → 1D), buy/sell signal markers, volume pane, custom HUD UI.
- **Pluggable multi-strategy framework** — register strategies behind a common
  trait; 8 example strategies included (gap fade, catalyst, pair/stat-arb,
  leveraged-ETF mean reversion, etc.).
- **SQLite trade journal** + CSV export with per-strategy attribution.
- **Cross-platform** — Linux / Windows / macOS via Tauri.

---

## Architecture

```
Frontend (ES Modules)            Backend (Rust / Tauri 2)
─────────────────────            ────────────────────────
index.html                       main.rs ............ app setup, async loops
modules/                         state.rs ........... ordered mutexes
  core.js ......... boot         smarttrend/ ........ engine + strategies
  chart.js ........ candles        sentinel/ ........ eval + dispatch loop
  positions.js .... pos table      strategies/ ...... pluggable strategies
  discovery.js .... scanner UI      bot/ ............. entry/exit/gates
  smarttrend.js ... engine UI     market/ ............ WS stream, scanner,
  journal.js ...... trade log                          indicators, catalyst
  config.js ....... settings     execution/ ......... orders, fills, recon
styles/hud.css .... HUD theme    risk/ .............. breaker, equity sync
                                 ops/ ............... persistence, journal
                                 crates/ ............ titan-indicators,
                                                      titan-risk, titan-types
```

**Mutex lock ordering** (enforced to prevent deadlocks):
`config → market → smarttrend → trading → risk → journal → system`

---

## Build

**Prerequisites:** Rust 1.75+, Node not required (vanilla JS frontend),
Tauri 2 system dependencies (e.g. `webkit2gtk`, `libsoup` on Linux).

```bash
# Development build
cd src-tauri && cargo build

# Optimized release build
./build.sh
# Binary: src-tauri/target/release/sentinel-trend
```

Run tests:

```bash
cd src-tauri
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Configuration

On first run, settings are created at
`~/.config/sentinel-trend/settings.json`.

Add your **own** Alpaca API keys (no keys are bundled):

```json
{
  "market": {
    "alpaca_key_id": "YOUR_KEY",
    "alpaca_secret": "YOUR_SECRET",
    "paper_mode": true,
    "sip_feed": false
  }
}
```

Optional: a Financial Modeling Prep (FMP) key enables earnings/float data in
the scanner.

**Start in paper mode** (`paper_mode: true`) until you have validated your own
strategy configuration.

---

## What's included / not included

**Included:** complete source (Rust backend + JS frontend), build scripts,
unit tests, this documentation.

**Not included:** API keys, any live account credentials, historical trade
data, or a profitable strategy. The bundled strategies are **examples** for
the framework and are not represented as profitable.

---

## 🖼️ UI Reference Screenshots (Compiled Desktop Confirmation)

> *Illustrative engineering confirmation only (virtual PAPER mode) — showing the compiled Rust + Tauri v2 desktop HUD; not financial advice or a performance claim.*

- 🌐 **Full Interactive UI Gallery:** [https://bot-tradingsoftware.com/#screenshots](https://bot-tradingsoftware.com/#screenshots)

![SentinelTrend Main HUD & Paper Positions](https://bot-tradingsoftware.com/img/screenshots/sentinel-trend-1-hud-chart.png)

![SentinelTrend Discovery Scanner & AutoUniverse Watchlist](https://bot-tradingsoftware.com/img/screenshots/sentinel-trend-2-discovery-scanner.png)

![SentinelTrend MERIDIAN Strategy Shadow Engine](https://bot-tradingsoftware.com/img/screenshots/sentinel-trend-3-meridian.png)

## ❤️ Support ("Good Samaritans") & Disclaimer

SentinelTrend is released **free of charge** under the [MIT License](LICENSE). If this ~67,000-LOC Rust/Tauri trading infrastructure helps you in your own personal projects, voluntary donations from "good samaritans" are welcome at:
- 🌐 **Open-Source Hub & Donations:** [https://bot-tradingsoftware.com/#donate](https://bot-tradingsoftware.com/#donate)

> **Strictly AS-IS / Zero Warranty / Zero Support:** Not financial advice. Trading involves substantial risk of loss. No API keys included. Donations are non-refundable gifts and do not purchase support, consulting, or warranties. See [DISCLAIMER.md](DISCLAIMER.md).
>
> **Additional Documentation:** Extended Romanian architecture & strategy manuals may be shared upon polite request if available, strictly AS-IS and without support.
