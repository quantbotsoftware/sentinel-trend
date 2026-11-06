#!/usr/bin/env python3
"""S18 — Pre-market mover scanner & AutoUniverse seeder.

Run this each morning between 06:30 and 09:25 ET to generate a list
of high-quality premarket gappers/RVOL leaders, then either:

  1. Pretty-print the suggestions for manual review (default).
  2. Pipe the symbols into the bot's pinned list using the running
     bot's config file (`~/.local/share/sentinel-trend/config/settings.json`)
     and a SIGHUP/restart so the AutoUniverse migration sees them
     on next boot — see `--apply` flag.

Why a separate script instead of a Rust loop?

  • Pre-market windows are operator-driven decisions. Reviewing the
    list manually before market open is risk-management, not noise.
  • Alpaca's premarket snapshot endpoint is rate-limited and we
    don't want the bot's hot path to depend on its availability.
  • Keeps the bot binary smaller and the architectural surface
    minimal: the bot trusts its watchlist; this tool curates it.

Required env (or pass via flags):
  SENTINEL_ALPACA_KEY     — Alpaca API key id
  SENTINEL_ALPACA_SECRET  — Alpaca API secret

Usage:
  ./premarket_scan.py                    # print top 20 gappers
  ./premarket_scan.py --top 10 --min-pct 5
  ./premarket_scan.py --apply --top 8    # write to settings.json
"""

import argparse
import json
import os
import shutil
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

CFG_PATH = Path.home() / ".local/share/sentinel-trend/config/settings.json"
ALPACA_DATA_BASE = "https://data.alpaca.markets"
ALPACA_REST_BASE = "https://paper-api.alpaca.markets"

# ANSI helpers (works in any modern terminal).
RESET = "\033[0m"
BOLD = "\033[1m"
DIM = "\033[2m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
RED = "\033[31m"
CYAN = "\033[36m"
MAGENTA = "\033[35m"


def load_credentials(args) -> tuple[str, str]:
    """Pull credentials from CLI or settings.json (in that order)."""
    key = args.api_key or os.environ.get("SENTINEL_ALPACA_KEY")
    sec = args.api_secret or os.environ.get("SENTINEL_ALPACA_SECRET")
    if not key or not sec:
        if CFG_PATH.exists():
            try:
                cfg = json.loads(CFG_PATH.read_text())
                key = key or cfg.get("market", {}).get("alpaca_key_id")
                sec = sec or cfg.get("market", {}).get("alpaca_secret")
            except Exception as e:
                print(
                    f"{YELLOW}Warning: settings.json read failed: {e}{RESET}",
                    file=sys.stderr,
                )
    if not key or not sec:
        sys.exit(
            f"{RED}ERROR: Alpaca credentials missing.{RESET} "
            "Provide via --api-key/--api-secret, env vars "
            "SENTINEL_ALPACA_KEY/SENTINEL_ALPACA_SECRET, or settings.json."
        )
    return key, sec


def alpaca_get(url: str, key: str, secret: str, params: dict | None = None) -> Any:
    """GET against Alpaca with auth + light retry. Returns parsed JSON."""
    if params:
        url = f"{url}?{urllib.parse.urlencode(params)}"
    req = urllib.request.Request(url)
    req.add_header("APCA-API-KEY-ID", key)
    req.add_header("APCA-API-SECRET-KEY", secret)
    last_err: Exception | None = None
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            if e.code in (429, 502, 503, 504) and attempt < 2:
                time.sleep(1.5 * (attempt + 1))
                last_err = e
                continue
            raise
        except Exception as e:  # noqa: BLE001
            last_err = e
            if attempt < 2:
                time.sleep(1.0 * (attempt + 1))
                continue
            raise
    raise RuntimeError(f"Alpaca GET exhausted retries: {last_err}")


def fetch_most_actives(key: str, secret: str, top: int = 50) -> list[dict]:
    """Alpaca screener — most-actives endpoint covers premarket activity."""
    url = f"{ALPACA_DATA_BASE}/v1beta1/screener/stocks/most-actives"
    params = {"by": "volume", "top": str(top)}
    data = alpaca_get(url, key, secret, params)
    return data.get("most_actives", []) or []


def fetch_movers(key: str, secret: str, top: int = 25) -> dict[str, list[dict]]:
    """Alpaca screener — top gainers + losers (premarket-aware)."""
    url = f"{ALPACA_DATA_BASE}/v1beta1/screener/stocks/movers"
    params = {"top": str(top)}
    data = alpaca_get(url, key, secret, params)
    return {
        "gainers": data.get("gainers", []) or [],
        "losers": data.get("losers", []) or [],
    }


def fetch_snapshots(symbols: list[str], key: str, secret: str) -> dict[str, dict]:
    """Bulk snapshot fetch — `latestTrade`, `dailyBar`, `prevDailyBar` per symbol."""
    if not symbols:
        return {}
    out: dict[str, dict] = {}
    BATCH = 100  # Alpaca limit
    for i in range(0, len(symbols), BATCH):
        chunk = symbols[i : i + BATCH]
        url = f"{ALPACA_DATA_BASE}/v2/stocks/snapshots"
        params = {"symbols": ",".join(chunk)}
        try:
            data = alpaca_get(url, key, secret, params)
            out.update(data or {})
        except Exception as e:  # noqa: BLE001
            print(
                f"{YELLOW}snapshot batch failed: {e}{RESET}",
                file=sys.stderr,
            )
    return out


def is_market_clock_open(key: str, secret: str) -> tuple[bool, str]:
    """Check Alpaca's market clock so we know if RTH started already."""
    try:
        clk = alpaca_get(f"{ALPACA_REST_BASE}/v2/clock", key, secret)
        return bool(clk.get("is_open")), clk.get("timestamp", "")
    except Exception as e:  # noqa: BLE001
        print(f"{YELLOW}clock fetch failed: {e}{RESET}", file=sys.stderr)
        return False, ""


def assemble_candidates(
    actives: list[dict],
    movers: dict[str, list[dict]],
    snapshots: dict[str, dict],
    args,
) -> list[dict]:
    """Merge sources, score each candidate, filter, sort by score DESC."""
    seen: set[str] = set()
    out: list[dict] = []

    def push(symbol: str, source: str, raw: dict) -> None:
        sym = symbol.upper()
        if sym in seen or not sym.isalpha():
            return
        snap = snapshots.get(sym, {})
        prev = snap.get("prevDailyBar", {}) or {}
        daily = snap.get("dailyBar", {}) or {}
        latest = snap.get("latestTrade", {}) or {}
        prev_close = float(prev.get("c") or raw.get("prev_close") or 0.0)
        cur_price = float(latest.get("p") or daily.get("c") or raw.get("price") or 0.0)
        if cur_price <= 0 or prev_close <= 0:
            return
        gap_pct = ((cur_price - prev_close) / prev_close) * 100.0
        # Filter — direction-tolerant (gainers: positive; losers: negative);
        # we keep both because shorts could still surface as longs after
        # premarket reversal. CLI flag --long-only restricts.
        if args.long_only and gap_pct < 0:
            return
        if abs(gap_pct) < args.min_pct:
            return
        if cur_price < args.min_price or cur_price > args.max_price:
            return

        prev_vol = float(prev.get("v") or 0.0)
        cur_vol = float(daily.get("v") or raw.get("volume") or 0.0)
        rvol = (cur_vol / prev_vol) if prev_vol > 0 else 0.0

        # Composite premarket score:
        #   - gap_pct        (capped 0..30)
        #   - rvol           (capped 0..10, weighted)
        #   - liquidity      (price * cur_vol, log-bonus)
        gap_capped = max(min(abs(gap_pct), 30.0), 0.0)
        rvol_capped = max(min(rvol, 10.0), 0.0)
        liq_score = (cur_price * cur_vol) / 1_000_000.0  # millions
        liq_log = (
            0.0 if liq_score <= 1.0 else min(20.0, 5.0 * (liq_score ** 0.25))
        )
        score = (gap_capped * 1.5) + (rvol_capped * 4.0) + liq_log

        out.append(
            {
                "symbol": sym,
                "source": source,
                "price": round(cur_price, 4),
                "prev_close": round(prev_close, 4),
                "gap_pct": round(gap_pct, 2),
                "rvol": round(rvol, 2),
                "volume": int(cur_vol),
                "score": round(score, 1),
            }
        )
        seen.add(sym)

    for r in actives:
        push(r.get("symbol", ""), "actives", r)
    for r in movers.get("gainers", []):
        push(r.get("symbol", ""), "gainer", r)
    for r in movers.get("losers", []):
        push(r.get("symbol", ""), "loser", r)

    out.sort(key=lambda x: x["score"], reverse=True)
    return out[: args.top]


def render_table(rows: list[dict]) -> None:
    if not rows:
        print(f"{DIM}No candidates passed filters.{RESET}")
        return
    header = (
        f"{BOLD}{'#':<3} {'SYM':<7} {'PRICE':>8} {'PREV':>8} "
        f"{'GAP%':>8} {'RVOL':>7} {'VOL':>12} {'SRC':<8} {'SCORE':>7}{RESET}"
    )
    print(header)
    print(f"{DIM}{'-' * len(header)}{RESET}")
    for i, r in enumerate(rows, 1):
        gap = r["gap_pct"]
        gap_color = GREEN if gap > 0 else RED
        rvol_color = MAGENTA if r["rvol"] >= 3.0 else (YELLOW if r["rvol"] >= 1.5 else "")
        score_color = GREEN if r["score"] >= 70 else (YELLOW if r["score"] >= 50 else "")
        print(
            f"{i:<3} {BOLD}{r['symbol']:<7}{RESET} "
            f"${r['price']:>7.4f} ${r['prev_close']:>7.4f} "
            f"{gap_color}{gap:>+7.2f}%{RESET} "
            f"{rvol_color}{r['rvol']:>6.2f}x{RESET} "
            f"{r['volume']:>12,} {DIM}{r['source']:<8}{RESET} "
            f"{score_color}{r['score']:>7.1f}{RESET}"
        )


def apply_to_settings(symbols: list[str]) -> None:
    """Replace `ui.favorites` with the new symbols. Backs up first."""
    if not CFG_PATH.exists():
        sys.exit(f"{RED}ERROR: settings.json not found at {CFG_PATH}{RESET}")
    backup = CFG_PATH.with_suffix(
        f".json.bak.{datetime.now().strftime('%Y%m%d_%H%M%S')}"
    )
    shutil.copy2(CFG_PATH, backup)
    try:
        cfg = json.loads(CFG_PATH.read_text())
    except Exception as e:
        sys.exit(f"{RED}ERROR: settings.json parse failed: {e}{RESET}")
    cfg.setdefault("ui", {})["favorites"] = symbols
    CFG_PATH.write_text(json.dumps(cfg, indent=2))
    print(
        f"{GREEN}Wrote {len(symbols)} symbols to ui.favorites in "
        f"{CFG_PATH}{RESET} (backup: {backup.name})"
    )
    print(
        f"{YELLOW}NOTE: restart sentinel-trend to load the new "
        f"watchlist (or wait for the next config save tick).{RESET}"
    )


def main() -> int:
    p = argparse.ArgumentParser(
        description="Pre-market mover scanner for SentinelTrend (S18 helper)"
    )
    p.add_argument("--top", type=int, default=20, help="Max candidates to surface")
    p.add_argument(
        "--min-pct",
        type=float,
        default=3.0,
        help="Minimum |gap%%| from prev close to qualify",
    )
    p.add_argument(
        "--min-price", type=float, default=1.0, help="Minimum price filter"
    )
    p.add_argument(
        "--max-price", type=float, default=100.0, help="Maximum price filter"
    )
    p.add_argument(
        "--long-only",
        action="store_true",
        help="Drop negative-gap movers (default: keep both)",
    )
    p.add_argument(
        "--apply",
        action="store_true",
        help="Write the top symbols into settings.json:ui.favorites",
    )
    p.add_argument("--api-key", help="Alpaca API key (env: SENTINEL_ALPACA_KEY)")
    p.add_argument(
        "--api-secret", help="Alpaca API secret (env: SENTINEL_ALPACA_SECRET)"
    )
    p.add_argument(
        "--json", action="store_true", help="Emit JSON to stdout instead of pretty table"
    )
    args = p.parse_args()

    key, secret = load_credentials(args)
    is_open, ts = is_market_clock_open(key, secret)
    print(
        f"{DIM}Alpaca clock @ {ts or 'n/a'} — "
        f"market is {'OPEN' if is_open else 'CLOSED (premarket / post-market / weekend)'}{RESET}"
    )

    print(f"{CYAN}Fetching most-actives + movers screeners…{RESET}")
    actives = fetch_most_actives(key, secret, top=50)
    movers = fetch_movers(key, secret, top=25)

    universe = sorted(
        {r["symbol"].upper() for r in actives}
        | {r["symbol"].upper() for r in movers.get("gainers", [])}
        | {r["symbol"].upper() for r in movers.get("losers", [])}
    )
    print(
        f"{DIM}Resolved {len(universe)} unique symbols across screener "
        f"sources; pulling snapshots…{RESET}"
    )
    snapshots = fetch_snapshots(universe, key, secret)
    candidates = assemble_candidates(actives, movers, snapshots, args)

    if args.json:
        json.dump(
            {
                "generated_at": datetime.now(timezone.utc).isoformat(),
                "candidates": candidates,
            },
            sys.stdout,
            indent=2,
        )
        print()
        return 0

    print(
        f"\n{BOLD}{CYAN}Pre-market candidates (top {len(candidates)} of universe "
        f"{len(universe)}){RESET}\n"
    )
    render_table(candidates)
    print()
    if not candidates:
        return 1

    if args.apply:
        symbols = [r["symbol"] for r in candidates]
        apply_to_settings(symbols)
    else:
        print(
            f"{DIM}Use --apply to write these {len(candidates)} symbols to "
            f"ui.favorites in settings.json (auto-pinned via S18 migration "
            f"on next bot start).{RESET}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
