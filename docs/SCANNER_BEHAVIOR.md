# Scanner & Watchlist — Operator Manual

**Audience:** day-trader operating SentinelTrend live; reviewer auditing
the scanner stack.  
**Scope:** explains every column, pill, and number the operator sees in
the `Discovery` panel — what it means, where it comes from, when to
trust it.  
**Source-of-truth pointers:**
[`src-tauri/src/market/scanner/scoring.rs`](../src-tauri/src/market/scanner/scoring.rs),
[`src-tauri/src/market/scanner/rvol.rs`](../src-tauri/src/market/scanner/rvol.rs),
[`src-tauri/src/market/scanner/prefilter.rs`](../src-tauri/src/market/scanner/prefilter.rs),
[`src-tauri/src/market/catalyst.rs`](../src-tauri/src/market/catalyst.rs),
[`src-tauri/src/market/alpaca_news.rs`](../src-tauri/src/market/alpaca_news.rs),
[`src-tauri/src/market/watchlist_refresh.rs`](../src-tauri/src/market/watchlist_refresh.rs),
[`src-tauri/src/commands/discovery.rs`](../src-tauri/src/commands/discovery.rs).

This document is normative for code-level invariants `SC1`–`SC10`
declared in `AGENTS.md` §5. If the UI or any downstream code disagrees
with the rules below, the code is wrong.

---

## 1. What changed pre-S11 vs post-S15

| Topic | Pre-S11 | Post-S15 |
|---|---|---|
| `current_price` source | `dailyBar.c` (yesterday's close) | `latestTrade.p` real-time, with `minuteBar.c` and `dailyBar.c` as fallbacks |
| RVOL formula | `volume_today / prev_volume` (suppresses early-day movers) | `volume_today / (prev_volume * session_progress)` time-adjusted |
| Catalyst column | none | `EARN`, `NEWS`, `HALT`, `GAP↑/GAP↓`, `VOL!` pills |
| Halt detection | none | Alpaca SIP `T="s"` status sub + RTH age heuristic |
| Watchlist between scans | symbols outside last `scan_results` returned `0.0` everywhere | dedicated 30 s refresh task, fresh real-time metrics per symbol |
| Compliance pre-filter | bot rejected at Gate 0 (wasted scan slot) | scanner skips `!asset.tradable` and shortable mismatch up-front |
| Stale data | invisible | `is_stale` flagged when `last_trade_age > 60 s` in RTH; row excluded from scoring, dimmed in watchlist |
| Bid / ask / spread | not surfaced | rendered, color-coded (`tight` / `wide`), sortable |
| Trade age | not surfaced | rendered, color-coded (`fresh` / `warn`), sortable |

The strategy itself (SmartTrend Momentum FSM) is unchanged. Scanner
Extreme is observability + data-quality hardening, not strategy logic.

---

## 2. Scanner table — 11 columns

The Discovery → Scan Results table.

| # | Column | Source | Notes |
|---|---|---|---|
| 1 | `SYMBOL` | scanner result | uppercased at boundary (S03) |
| 2 | `CATALYST` | `catalyst_pills` | see §4 |
| 3 | `PRICE` | `latestTrade.p` (fallback chain SC1) | real-time, not yesterday's close |
| 4 | `CHG%` | `(price - prev_close) / prev_close * 100` | green for `≥ 0`, red otherwise |
| 5 | `VOL` | `dailyBar.v` (fallback `minuteBar.v`) | accumulated since session open |
| 6 | `RVOL` | time-adjusted (default; see §6) | `≥ 2.0` highlighted "hot" |
| 7 | `BID` | `latestQuote.bp` | `0` when no fresh NBBO available |
| 8 | `ASK` | `latestQuote.ap` | `0` when no fresh NBBO available |
| 9 | `SPREAD` | `(ask - bid) / mid × 10_000` (bps) | colored: `< 10 bps` green, `≥ 50 bps` red |
| 10 | `AGE` | `now_et - latestTrade.t` (seconds) | `< 5 s` green, `≥ 30 s` orange |
| 11 | `BUY` | action button | opens manual order form for the symbol |

**Stale row dimming.** When `is_stale = true` (RTH only,
`last_trade_age > 60 s`), the row is dimmed via `.st-stale-row` (opacity
≈ 0.55). In the scanner, stale candidates are *excluded* from results
entirely — so a dimmed scanner row is rare and signals a bug; in the
watchlist (§3), stale rows still appear so the operator sees the gap.

**Sort order.** Default sort is `score` descending. `data-sort` is set
on every column header; click toggles direction.

---

## 3. Watchlist table — 14 columns

The Discovery → Watchlist table. Authoritative source is the 30 s
refresh task that writes Tier-B6 `watchlist_metrics`; on cold-start the
table falls back to `scan_results` + `last_prices`.

| # | Column | Source | Notes |
|---|---|---|---|
| 1 | trend arrow | `change_percent` sign | green ▲ up, red ▼ down |
| 2 | `SYMBOL` (with `E` badge) | favorites + `MarketState.earnings_today` | `E` = earnings today/tomorrow |
| 3 | `CATALYST` | `catalyst_pills` (recomputed in `build_item` with correct `has_earnings`) | see §4 |
| 4 | `PRICE` | live `last_prices[symbol]` if streaming, else B6 `price` | real-time |
| 5 | `CHG%` | B6 `change_percent` | re-derived from `latestTrade.p - prev_close` |
| 6 | `VOL` | B6 `volume` | accumulated since session open |
| 7 | `RVOL` | B6 `rvol` (TimeAdjusted by default) | bold green when `> 2.0` |
| 8 | `GAP%` | `(open - prev_close) / prev_close * 100` | persistent across the day |
| 9 | `BID` / 10 `ASK` | B6 `bid` / `ask` | `—` when no fresh quote |
| 11 | `SPREAD` | bps, S14 coloring | tight / wide |
| 12 | `AGE` | trade age, S14 coloring | fresh / warn |
| 13 | `STSCORE` | `MarketState.fsm[sym].score` | SmartTrend FSM combined score |
| 14 | `×` | remove button | `remove_from_watchlist` command |

**Refresh cadence.** Background task ticks every **30 s**. Each tick
follows the lock-ordering protocol documented in
[`watchlist_refresh.rs`](../src-tauri/src/market/watchlist_refresh.rs)
(Tier-A read → drop → network I/O → Tier-B5 halts → compute → Tier-B6
swap → emit-after-drop). The frontend listens on `watchlist-update` and
merges fresh metrics into the existing rows while preserving Tier-A
fields (`has_earnings`, `st_score`, `st_state`).

**Cold-start.** During the first 30 s after launch (or for a freshly
added favorite), B6 may not have an entry yet. `commands/discovery.rs::
build_item` falls back to: `last_prices[symbol]` for price, the
matching `scan_results` row for change/volume/rvol/gap, and a fresh
`is_halted_combined` call. `bid` / `ask` / `spread_bps` are `0` in this
fallback path (no quote source).

---

## 4. Catalyst pills — cheat sheet

Pills are tiny labels in the `CATALYST` column. They render via
`renderCatalystPills(pills)` in
[`scan-results.js`](../src/modules/discovery/scan-results.js) (also
imported by `watchlist.js` per F1 / SC9). Every label runs through
`escapeHtml` defensively.

| Pill | When it appears | CSS token (SC10) |
|---|---|---|
| `HALT` | Symbol is halted (status sub OR RTH age heuristic). Short-circuits all other pills. | `var(--accent-orange)` |
| `EARN` | Symbol on FMP earnings calendar for today/tomorrow. | `var(--accent-gold)` |
| `NEWS` | At least one Alpaca news item in the last `news_lookback_minutes` (default 240). | `var(--accent-cyan)` |
| `GAP↑` | `gap_pct ≥ catalyst.weights.gap_threshold` (default 5 %). | `var(--accent-green)` |
| `GAP↓` | `gap_pct ≤ -gap_threshold`. | `var(--accent-red)` |
| `VOL!` | `rvol ≥ catalyst.weights.rvol_threshold` (default 3×). | `var(--accent-magenta)` |

Pill ordering for non-halted rows: `[EARN, NEWS, GAP↑/↓, VOL!]` (declared
in `CatalystFlags::pills()`).

---

## 5. Catalyst score — formula and weights

`CatalystFlags::compute(input, weights)` in
[`catalyst.rs`](../src-tauri/src/market/catalyst.rs):

```
if input.is_halted: return { halted: true, score: 0.0 }   # short-circuit

gap_dir =  Up    if gap_pct ≥  weights.gap_threshold
           Down  if gap_pct ≤ -weights.gap_threshold
           None  otherwise
vol_spike = rvol ≥ weights.rvol_threshold

score = 0
if has_earnings:   score += w_earn   # default 30
if has_recent_news: score += w_news  # default 25
if gap_dir != None: score += w_gap   # default 20
if vol_spike:      score += w_vol    # default 25
score = clamp(score, 0, 100)
```

**Default `CatalystWeights` (from `RuntimeConfig.catalyst.weights`):**

| Field | Default | Meaning |
|---|---|---|
| `w_earn` | 30 | Bonus for earnings today/tomorrow. |
| `w_news` | 25 | Bonus for at least one fresh news item. |
| `w_halt` | -50 | **Reserved** — not consumed today (halt short-circuits). Retained for future post-halt cooldown telemetry. |
| `w_gap` | 20 | Bonus when gap exceeds `gap_threshold`. |
| `w_vol` | 25 | Bonus when RVOL exceeds `rvol_threshold`. |
| `gap_threshold` | 5.0 (%) | Gap up/down classifier. |
| `rvol_threshold` | 3.0 (×) | Volume-spike classifier. |

Score is clamped to `[0, 100]`. Maximum non-halted score = `30 + 25 + 20
+ 25 = 100`.

**News lookback.** `RuntimeConfig.catalyst.news_lookback_minutes`
(default `240` = 4 h). The catalyst news refresh loop fetches once
every 5 min and caches per-symbol with TTL = 5 min. A pill is set when
`build_news_map` returns at least one item for the symbol.

---

## 6. RVOL — Naive vs TimeAdjusted vs Historical

`RvolMethod` is set on `ScanParams.rvol_method` and persisted on the
`scanner` block of `settings.json`.

| Method | Formula | When to use |
|---|---|---|
| `TimeAdjusted` (default; `SC2`) | `vol_today / (prev_volume × session_progress)` | Production. Honest at 8 AM and at 16:00. |
| `Naive` | `vol_today / prev_volume` | Regression study only. Suppresses early-session movers. |
| `Historical` | falls through to `TimeAdjusted` until S12.5 lands | Forward-compat placeholder. |

### `compute_session_progress(now_et)` — piecewise-linear

| ET wallclock | Progress | Notes |
|---|---|---|
| Saturday/Sunday | `0.0` | Hard zero. |
| `< 04:00` | `0.0` | Pre-premarket. |
| `04:00 → 09:30` | linear `0.00 → 0.08` | Premarket (5.5 h, 8 % of expected day volume). |
| `09:30 → 16:00` | linear `0.08 → 0.92` | Regular trading hours (6.5 h, 84 %). |
| `16:00 → 20:00` | linear `0.92 → 1.00` | Extended hours (4 h, 8 %). |
| `≥ 20:00` | `1.0` | After-hours close. |

**DST**: handled by `chrono_tz::US::Eastern`; NYSE never transitions
during a session (DST switches at 02:00 Sunday).

### Worked example — TSLA premarket 8 AM

```
volume_today  = 20_000_000
prev_volume   = 50_000_000
now_et        = 08:00 ET (premarket, weekday)

session_progress(08:00) = (240 - 240) / (570 - 240) × 0.08 + 0.0  →  0.058
expected      = 50_000_000 × 0.058 = 2_910_000
rvol          = 20_000_000 / 2_910_000 ≈ 6.87
```

Naive RVOL would yield `20M / 50M = 0.4` and fail a typical
`rvol_min = 2.0` floor, suppressing a real morning mover. TimeAdjusted
gives the operator a usable signal.

### IPO / weekend safety

`time_adjusted_rvol` returns `1.0` when:
- `prev_volume ≤ 0` (brand-new IPO),
- weekend,
- `progress ≤ 0.001` (pre-04:00),
- division produces a non-finite or non-positive result.

No NaN / panic / negative RVOL ever surfaces.

---

## 7. Stale criteria

`is_stale` is set when:
```
in_rth(now_et) && last_trade_age_seconds > 60
```

Where `in_rth = weekday && 09:30 ≤ now_et < 16:00 ET`. The threshold is
the SCANNER_ROADMAP §Risk-off default — 60 s is conservative for SIP
data which normally updates several times per second.

**Behavior:**
- Scanner: stale candidates are *excluded* from `quant_prefilter`
  results entirely (carry-forward of the SCANNER_ROADMAP §S12 decision).
- Watchlist: stale rows still appear, dimmed via `.st-stale-row`
  (operator awareness > exclusion).

Premarket and extended-hours rows can have arbitrary trade age without
being stale by construction — there's no NBBO obligation outside RTH.

---

## 8. Halt detection — sources

`is_halted_combined` (`SC6`) OR-combines two signals:

### Source 1 — Alpaca SIP `T="s"` status sub (primary)

The streamer subscribes to `"statuses": ["*"]` and dispatches each
status message to `MarketDataState::apply_status_message`. The `sc`
field is the machine-readable status code:

| `sc` | Meaning | Treatment |
|---|---|---|
| `H` | Trading halt (NYSE/Nasdaq) | halt |
| `2` | Halt — SEC suspension | halt |
| `P` | UTP volatility trading pause / LULD | halt |
| `Q` | UTP quote resumption | resume (clears halt) |
| `T` | UTP trade resumption | resume |
| `R` | Generic resume | resume |
| `3` | CTA resume | resume |
| anything else | unknown | logged at trace, ignored |

`sm` (status message) and `rm` (reason message) are stored in
`HaltState.reason` for UI display. `HaltSource::StatusSub` flags the
source.

### Source 2 — RTH age heuristic (fallback)

When the status sub hasn't reported a halt but a trade hasn't arrived
in over **120 s** during RTH, the symbol is *treated* as halted at
consume-time without writing `MarketDataState.halts` (primary signal
observability stays clean). `HaltSource::HeuristicAge` is reserved for
the type but never actually persisted.

**Why both:** SIP can be late or missed during reconnects. The
heuristic catches the gap. They never cancel each other — the OR
returns `true` if either says halt.

---

## 9. Tooltip glossary

Quick definitions for column headers:

| Term | Definition |
|---|---|
| `BID` | Best bid (`latestQuote.bp`). The price at which a market participant is willing to buy. |
| `ASK` | Best offer (`latestQuote.ap`). The price at which a market participant is willing to sell. |
| `SPREAD` | `((ask - bid) / mid) × 10_000` in basis points. Tight `< 10 bps`, wide `≥ 50 bps`. Liquidity proxy. |
| `AGE` | Seconds since the last consolidated trade arrived. SIP is fast; in RTH normal `AGE` is `< 1 s`. |
| `RVOL` | Relative volume — see §6. `5×` means today's projected pace is 5× the previous day. |
| `GAP%` | Open vs previous close (% of prev close). Persistent across the day. |
| `STSCORE` | SmartTrend FSM combined score (`fsm[sym].score`). Drives entry decisions. Not a catalyst score. |

---

## 10. Lock ordering snapshot (SC8)

| Tier | Index | Lock | Used by |
|---|---|---|---|
| A | A1 | `config` | every entry point |
| A | A2 | `market` | scanner results, earnings calendar |
| A | A3 | `smarttrend` | FSM rows |
| A | A4 | `trading` | unrelated to scanner |
| A | A5 | `risk` | unrelated to scanner |
| A | A6 | `journal` | unrelated to scanner |
| A | A7 | `system` | watchdog ticks |
| B | B1 | `last_prices` | live price column |
| B | B2 | `cvd` | unrelated to scanner |
| B | B3 | `candle_buffers` | unrelated to scanner |
| B | B4 | `indicator_cache` | unrelated to scanner |
| B | B5 | `halts` (S13) | catalyst halt resolution |
| B | B6 | `watchlist_metrics` (S14) | watchlist live data |

Tier-A and Tier-B are **never** held simultaneously. Within a tier,
ascending order. Per-tick proofs in `s13_completion.md` and
`s14_completion.md`.

---

## 11. Source pointers (for code readers)

| Concern | File |
|---|---|
| `BarMetrics` / `extract_bar_metrics` / `in_rth` | [`market/scanner/scoring.rs`](../src-tauri/src/market/scanner/scoring.rs) |
| `RvolMethod` / `time_adjusted_rvol` / `compute_session_progress` | [`market/scanner/rvol.rs`](../src-tauri/src/market/scanner/rvol.rs) |
| Compliance gate, RVOL dispatch, catalyst wiring | [`market/scanner/prefilter.rs`](../src-tauri/src/market/scanner/prefilter.rs) |
| `CatalystFlags`, `pills`, `is_halted_combined` | [`market/catalyst.rs`](../src-tauri/src/market/catalyst.rs) |
| News API client + 5 min cache | [`market/alpaca_news.rs`](../src-tauri/src/market/alpaca_news.rs) |
| `HaltState` / `apply_status_message` / `WatchlistMetrics` | [`market/market_state.rs`](../src-tauri/src/market/market_state.rs) |
| 30 s refresh task | [`market/watchlist_refresh.rs`](../src-tauri/src/market/watchlist_refresh.rs) |
| `get_watchlist` (B6 primary, scan-results fallback) | [`commands/discovery.rs`](../src-tauri/src/commands/discovery.rs) |
| Frontend pills + spread/age coloring | [`src/modules/discovery/scan-results.js`](../src/modules/discovery/scan-results.js) |
| Frontend watchlist + 30 s listener | [`src/modules/discovery/watchlist.js`](../src/modules/discovery/watchlist.js) |
| CSS pill / stale / spread / age tokens | [`src/styles/hud.css`](../src/styles/hud.css) (`.st-pill*`, `.st-stale-row`, `.st-spread*`, `.st-age*`) |

---

## 12. What the scanner does NOT do

For clarity (and to head off feature requests):

- **No predictive ML.** No genetic algorithms. No backtest parity score.
- **No real Historical RVOL** yet. `RvolMethod::Historical` is reserved
  for S12.5 (`MinuteVolumeCache`); today it falls through to
  TimeAdjusted.
- **No news ticker UI.** News is consumed only via the `NEWS` pill.
- **No options / crypto.** US equities only, single-leg long/short.
- **No bot decisions in the scanner.** Bot Gate 0 still applies; the
  scanner's compliance pre-filter is a fast-path eliminator, not a
  trading authority.

---

*Document generated 2026-04-30 as part of S15 (Scanner Extreme final
validation). For implementation-level invariants see `AGENTS.md` §5
(SC1–SC10).*
