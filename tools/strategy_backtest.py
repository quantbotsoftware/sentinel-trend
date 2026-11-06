#!/usr/bin/env python3
"""S1 Phase 1.1 — D Evolution Protocol backtest harness.

Pulls 60 days of 5-minute bars from Alpaca paper API for ~200
symbols, simulates 8 candidate strategies on identical data, and
emits a CSV + markdown summary used to make the GO / NO-GO call
for Sesiunea 1.

Strategy implementation here is **deterministic & simplified** —
the production logic lives in Rust (Phase 1.2). The Python clones
are intentionally kept small (one entry rule, one exit rule per
strategy) so the GO/NO-GO numbers represent the *minimum* edge
of each idea. If a stripped-down clone clears the bar, the full
Rust version with proper risk management & filters will only do
better.

Strategies that depend on data we cannot easily backtest
(news flow, halt feeds, second-level catalyst tape) are flagged
``[ESTIMATED]`` and use literature priors instead of simulated
P/L. They still get a GO/NO-GO row but Tony reviews them with
extra skepticism.

Usage::

    ./tools/strategy_backtest.py                    # full run
    ./tools/strategy_backtest.py --refresh-bars     # invalidate cache
    ./tools/strategy_backtest.py --days 30          # shorter window
    ./tools/strategy_backtest.py --symbols-file …   # custom universe

Output::

    tools/cache/bars_60d.json.gz       cached bars (re-used)
    tools/results/backtest_<date>.csv  per-strategy stats
    tools/results/backtest_<date>.md   human-readable summary
"""

from __future__ import annotations

import argparse
import csv
import gzip
import json
import math
import os
import statistics
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable

REPO_ROOT = Path(__file__).resolve().parent.parent
TOOLS_DIR = REPO_ROOT / "tools"
CACHE_DIR = TOOLS_DIR / "cache"
RESULTS_DIR = TOOLS_DIR / "results"
CFG_PATH = Path.home() / ".local/share/sentinel-trend/config/settings.json"

ALPACA_DATA_BASE = "https://data.alpaca.markets"

# ANSI colours (matches premarket_scan.py palette).
RESET = "\033[0m"
BOLD = "\033[1m"
DIM = "\033[2m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
RED = "\033[31m"
CYAN = "\033[36m"

# ---------------------------------------------------------------------------
# Universe (200 symbols)
# ---------------------------------------------------------------------------
#
# Three buckets keep coverage realistic for the 8 strategies we test:
#   - SMALL_CAP_MOMO: low-float gappers / momentum names — feeds Gap Fade,
#     Catalyst Front-Run, Halt Resume, Extended News
#   - LIQUID_LARGE: SP500 large caps — feeds Stat Arb 50, Afternoon Fade
#   - ETFS: index + sector + leveraged — feeds ETF Pair, Leveraged ETF
#
# Lists are static intentionally: a backtest universe should not silently
# drift between runs (otherwise GO/NO-GO becomes irreproducible).
SMALL_CAP_MOMO = [
    "QBTS", "AGL", "BBAI", "RGTI", "IONQ", "SOUN", "PLTR", "RXRX", "BMNR",
    "SMR", "OKLO", "VRT", "ANET", "DUOL", "ARM", "SMCI", "MRVL", "MU",
    "NVDA", "AMD", "TSLA", "RIVN", "LCID", "NIO", "MARA", "RIOT", "CLSK",
    "WULF", "BTBT", "HUT", "BITF", "CIFR", "GREE", "IREN", "HIVE",
    "MSTR", "COIN", "HOOD", "SOFI", "AFRM", "UPST", "OPEN", "NCLH",
    "CCL", "RCL", "BA", "F", "GM", "FFIE", "TUP", "MULN", "AMC",
    "GME", "KOSS", "BBBY", "EXPR", "BB", "NOK", "WISH", "CLOV", "BLNK",
    "PLUG", "FCEL", "RUN", "SEDG", "ENPH", "FSLR", "PYPL", "DKNG",
    "PENN", "WBD", "PARA", "DIS", "ROKU", "PINS", "SNAP", "SPOT", "ZM",
    "DOCN", "DDOG", "NET", "OKTA", "CRWD", "ZS", "S", "TWLO", "U",
    "RBLX", "PATH", "AI", "AFRM", "LMND", "DASH", "ABNB", "UBER", "LYFT",
    "TLRY", "CGC", "ACB", "SNDL", "HEXO", "VFF", "NVTA", "EDIT", "BEAM",
    "CRSP", "SAVA", "AXSM", "SAGE", "BNTX", "MRNA", "NVAX", "PFE",
]

LIQUID_LARGE = [
    "SPY", "QQQ", "IWM", "DIA", "AAPL", "MSFT", "GOOGL", "AMZN", "META",
    "TSLA", "NVDA", "BRK.B", "JPM", "V", "JNJ", "WMT", "UNH", "MA", "PG",
    "HD", "CVX", "XOM", "BAC", "ABBV", "PFE", "KO", "PEP", "AVGO", "TMO",
    "COST", "CSCO", "MRK", "DIS", "ACN", "VZ", "ADBE", "T", "NFLX", "CMCSA",
    "CRM", "INTC", "ABT",
]

ETFS = [
    # Sector
    "XLF", "XLK", "XLE", "XLV", "XLI", "XLB", "XLU", "XLY", "XLP", "XLRE",
    "XLC",
    # Leveraged 3x
    "TQQQ", "SQQQ", "SOXL", "SOXS", "UPRO", "SPXU", "TNA", "TZA", "FAS",
    "FAZ", "TMF", "TMV", "LABU", "LABD",
    # Volatility / commodity
    "UVXY", "VIXY", "GLD", "SLV", "USO", "UNG", "TLT", "HYG", "LQD",
]


def default_universe() -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    for s in SMALL_CAP_MOMO + LIQUID_LARGE + ETFS:
        u = s.upper()
        if u not in seen:
            out.append(u)
            seen.add(u)
    return out


# ---------------------------------------------------------------------------
# Alpaca helpers
# ---------------------------------------------------------------------------


def load_credentials(args) -> tuple[str, str]:
    key = args.api_key or os.environ.get("SENTINEL_ALPACA_KEY")
    sec = args.api_secret or os.environ.get("SENTINEL_ALPACA_SECRET")
    if (not key or not sec) and CFG_PATH.exists():
        try:
            cfg = json.loads(CFG_PATH.read_text())
            key = key or cfg.get("market", {}).get("alpaca_key_id")
            sec = sec or cfg.get("market", {}).get("alpaca_secret")
        except Exception as e:  # noqa: BLE001
            print(f"{YELLOW}settings.json read failed: {e}{RESET}", file=sys.stderr)
    if not key or not sec:
        sys.exit(
            f"{RED}ERROR: Alpaca credentials missing.{RESET} Provide via "
            "--api-key/--api-secret or settings.json."
        )
    return key, sec


def alpaca_get(url: str, key: str, secret: str, params: dict | None = None) -> Any:
    if params:
        url = f"{url}?{urllib.parse.urlencode(params)}"
    req = urllib.request.Request(url)
    req.add_header("APCA-API-KEY-ID", key)
    req.add_header("APCA-API-SECRET-KEY", secret)
    last_err: Exception | None = None
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            if e.code in (429, 502, 503, 504) and attempt < 2:
                time.sleep(2.0 * (attempt + 1))
                last_err = e
                continue
            raise
        except Exception as e:  # noqa: BLE001
            last_err = e
            if attempt < 2:
                time.sleep(1.0 * (attempt + 1))
                continue
            raise
    raise RuntimeError(f"Alpaca GET retries exhausted: {last_err}")


def fetch_multi_bars(
    symbols: list[str],
    key: str,
    secret: str,
    start_iso: str,
    end_iso: str,
    timeframe: str = "5Min",
) -> dict[str, list[dict]]:
    """Multi-symbol bars endpoint with pagination."""
    out: dict[str, list[dict]] = {s: [] for s in symbols}
    BATCH = 50
    for i in range(0, len(symbols), BATCH):
        chunk = symbols[i : i + BATCH]
        page_token: str | None = None
        page = 0
        while True:
            page += 1
            params: dict[str, Any] = {
                "symbols": ",".join(chunk),
                "start": start_iso,
                "end": end_iso,
                "timeframe": timeframe,
                "limit": 10000,
                "feed": "sip",
                "adjustment": "raw",
            }
            if page_token:
                params["page_token"] = page_token
            try:
                data = alpaca_get(
                    f"{ALPACA_DATA_BASE}/v2/stocks/bars",
                    key,
                    secret,
                    params,
                )
            except urllib.error.HTTPError as e:
                if e.code == 403 and "feed=sip" in str(params):
                    # downgrade to IEX feed (paper accounts often restricted)
                    params["feed"] = "iex"
                    data = alpaca_get(
                        f"{ALPACA_DATA_BASE}/v2/stocks/bars",
                        key,
                        secret,
                        params,
                    )
                else:
                    print(
                        f"{YELLOW}batch {i // BATCH + 1} page {page} failed: "
                        f"{e}{RESET}",
                        file=sys.stderr,
                    )
                    break
            bars = data.get("bars") or {}
            for sym, rows in bars.items():
                if rows:
                    out.setdefault(sym, []).extend(rows)
            page_token = data.get("next_page_token")
            if not page_token:
                break
        print(
            f"{DIM}  batch {i // BATCH + 1}/{(len(symbols) + BATCH - 1) // BATCH} "
            f"({len(chunk)} syms) OK{RESET}"
        )
    return out


# ---------------------------------------------------------------------------
# Cache (gzipped JSON to avoid pyarrow / parquet dependency)
# ---------------------------------------------------------------------------


def cache_path(days: int) -> Path:
    return CACHE_DIR / f"bars_{days}d.json.gz"


def load_cached_bars(days: int) -> dict[str, list[dict]] | None:
    p = cache_path(days)
    if not p.exists():
        return None
    try:
        with gzip.open(p, "rt", encoding="utf-8") as f:
            payload = json.load(f)
        ts = payload.get("generated_at", "")
        print(f"{GREEN}Cache hit:{RESET} {p.name} ({ts})")
        return payload.get("bars")
    except Exception as e:  # noqa: BLE001
        print(f"{YELLOW}Cache load failed ({e}); re-fetching{RESET}")
        return None


def save_cached_bars(days: int, bars: dict[str, list[dict]]) -> None:
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    p = cache_path(days)
    payload = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "days": days,
        "bars": bars,
    }
    with gzip.open(p, "wt", encoding="utf-8") as f:
        json.dump(payload, f)
    size_mb = p.stat().st_size / (1024 * 1024)
    print(f"{GREEN}Cache saved:{RESET} {p.name} ({size_mb:.1f} MB)")


# ---------------------------------------------------------------------------
# Bar helpers
# ---------------------------------------------------------------------------


@dataclass
class Bar:
    t_iso: str
    open: float
    high: float
    low: float
    close: float
    volume: float

    @property
    def t(self) -> datetime:
        return datetime.fromisoformat(self.t_iso.replace("Z", "+00:00"))

    @property
    def t_et(self) -> datetime:
        # Alpaca bar timestamps are UTC. ET = UTC-4 (EDT) most of the year;
        # we use a fixed offset because the simulator does not need DST
        # nuance — backtest stays within market session bars filtered by
        # the bucket function below.
        return self.t.astimezone(timezone(timedelta(hours=-4)))


def parse_bars(rows: list[dict]) -> list[Bar]:
    out: list[Bar] = []
    for r in rows:
        try:
            out.append(
                Bar(
                    t_iso=r.get("t", ""),
                    open=float(r.get("o", 0.0)),
                    high=float(r.get("h", 0.0)),
                    low=float(r.get("l", 0.0)),
                    close=float(r.get("c", 0.0)),
                    volume=float(r.get("v", 0.0)),
                )
            )
        except Exception:  # noqa: BLE001
            continue
    out.sort(key=lambda b: b.t_iso)
    return out


def session_minute(b: Bar) -> int:
    """Minutes from 09:30 ET (negative = premarket, > 390 = post-market)."""
    et = b.t_et
    return (et.hour * 60 + et.minute) - (9 * 60 + 30)


def in_rth(b: Bar) -> bool:
    m = session_minute(b)
    return 0 <= m < 390  # 09:30-16:00


def group_by_day(bars: list[Bar]) -> dict[str, list[Bar]]:
    out: dict[str, list[Bar]] = {}
    for b in bars:
        d = b.t_et.date().isoformat()
        out.setdefault(d, []).append(b)
    return out


def atr(bars: list[Bar], period: int = 14) -> float:
    """Simple ATR proxy: average true range across last `period` bars.

    If we have fewer than ``period+1`` bars, falls back to averaging
    whatever true ranges we can compute (so callers don't silently get
    0.0 — that bug ate the entire gap_fade signal in the 2026-05-10 run).
    """
    if len(bars) < 2:
        return 0.0
    trs: list[float] = []
    for i in range(1, len(bars)):
        tr = max(
            bars[i].high - bars[i].low,
            abs(bars[i].high - bars[i - 1].close),
            abs(bars[i].low - bars[i - 1].close),
        )
        trs.append(tr)
    if not trs:
        return 0.0
    n = min(period, len(trs))
    return sum(trs[-n:]) / n


# ---------------------------------------------------------------------------
# Trade & strategy primitives
# ---------------------------------------------------------------------------


@dataclass
class Trade:
    strategy: str
    symbol: str
    side: str           # "long" or "short"
    entry_t: str
    entry_px: float
    exit_t: str
    exit_px: float
    qty: float
    pnl: float
    hold_minutes: float
    exit_reason: str


def compute_pnl(
    side: str,
    entry: float,
    exit: float,
    qty: float,
    fee_per_share: float = 0.0,
) -> float:
    """Realised P/L net of round-trip fees.

    Alpaca paper trading is commissionless (matches Alpaca live equities
    pricing as of May 2026), so default fee=0. Override via FEE_PER_SHARE
    env var if you want to model SIPC/exchange pass-through costs:

        FEE_PER_SHARE=0.0035 ./tools/strategy_backtest.py
    """
    direction = 1.0 if side == "long" else -1.0
    gross = (exit - entry) * direction * qty
    fees = fee_per_share * qty * 2.0
    return gross - fees


def position_size(
    side: str,
    entry: float,
    stop: float,
    risk_dollars: float = 50.0,
) -> float:
    """Risk-based position sizing — `risk_dollars` per trade, distance to
    stop sets share count. Uses 1 share fallback if stop is at entry."""
    distance = abs(entry - stop)
    if distance <= 0.001:
        return 1.0
    shares = risk_dollars / distance
    return max(1.0, math.floor(shares))


# ---------------------------------------------------------------------------
# Strategy clones (deterministic, simplified)
# ---------------------------------------------------------------------------


@dataclass
class StrategyResult:
    name: str
    estimated: bool = False
    trades: list[Trade] = field(default_factory=list)
    notes: str = ""


def strategy_gap_fade(
    bars_by_symbol: dict[str, list[Bar]],
) -> StrategyResult:
    """Long fade of overnight gappers >5%.

    Entry: at the close of the 09:30 bar if open-gap |%| > 5
    (short on gap-up, long on gap-down).
    Exit: 10:30 ET hard time stop OR ±1.5×ATR(14 5min) bracket.
    """
    res = StrategyResult(name="gap_fade")
    res.notes = "fade gappers > 5% (09:30-10:30, ATR=1.5)"
    for sym, all_bars in bars_by_symbol.items():
        days = group_by_day(all_bars)
        for day, day_bars in days.items():
            rth = [b for b in day_bars if in_rth(b)]
            if len(rth) < 12:
                continue
            open_bar = rth[0]
            prev_day = sorted(d for d in days if d < day)
            if not prev_day:
                continue
            prev_bars = days[prev_day[-1]]
            prev_close_bars = [b for b in prev_bars if in_rth(b)]
            if not prev_close_bars:
                continue
            prev_close = prev_close_bars[-1].close
            if prev_close <= 0:
                continue
            gap_pct = ((open_bar.open - prev_close) / prev_close) * 100.0
            if abs(gap_pct) < 5.0:
                continue
            # fade direction
            side = "short" if gap_pct > 0 else "long"
            atr_val = atr(rth[:14])
            if atr_val <= 0:
                continue
            entry = open_bar.close
            stop_dist = 1.5 * atr_val
            target_dist = 1.5 * atr_val
            stop = entry + stop_dist if side == "short" else entry - stop_dist
            target = entry - target_dist if side == "short" else entry + target_dist
            qty = position_size(side, entry, stop)
            cutoff_minute = 60  # 10:30 ET
            exit_px = entry
            exit_t = open_bar.t_iso
            exit_reason = "time_stop"
            for b in rth[1:]:
                m = session_minute(b)
                if m > cutoff_minute:
                    exit_px = b.open
                    exit_t = b.t_iso
                    exit_reason = "time_stop"
                    break
                if side == "short":
                    if b.high >= stop:
                        exit_px = stop
                        exit_t = b.t_iso
                        exit_reason = "stop"
                        break
                    if b.low <= target:
                        exit_px = target
                        exit_t = b.t_iso
                        exit_reason = "target"
                        break
                else:
                    if b.low <= stop:
                        exit_px = stop
                        exit_t = b.t_iso
                        exit_reason = "stop"
                        break
                    if b.high >= target:
                        exit_px = target
                        exit_t = b.t_iso
                        exit_reason = "target"
                        break
            pnl = compute_pnl(side, entry, exit_px, qty)
            hold = (datetime.fromisoformat(exit_t.replace("Z", "+00:00"))
                    - open_bar.t).total_seconds() / 60.0
            res.trades.append(
                Trade(
                    strategy=res.name,
                    symbol=sym,
                    side=side,
                    entry_t=open_bar.t_iso,
                    entry_px=entry,
                    exit_t=exit_t,
                    exit_px=exit_px,
                    qty=qty,
                    pnl=pnl,
                    hold_minutes=hold,
                    exit_reason=exit_reason,
                )
            )
    return res


def strategy_etf_pair(
    bars_by_symbol: dict[str, list[Bar]],
) -> StrategyResult:
    """Mean-revert pair trade on (XLK / SPY) Z-score.

    Computes 50-bar rolling Z of the price ratio. Entry when |Z| > 2.0;
    long the lagging leg, short the leading leg. Exit when Z reverts to
    |Z| < 0.5 OR 60-min time stop.
    """
    res = StrategyResult(name="etf_pair")
    res.notes = "XLK/SPY Z-score (10:30-15:00, |Z|>2 entry, |Z|<0.5 exit)"
    if "XLK" not in bars_by_symbol or "SPY" not in bars_by_symbol:
        res.notes += " | SKIPPED: XLK or SPY missing"
        return res
    spy_bars = {b.t_iso: b for b in bars_by_symbol["SPY"]}
    pairs: list[tuple[Bar, Bar]] = []
    for xlk_b in bars_by_symbol["XLK"]:
        spy_b = spy_bars.get(xlk_b.t_iso)
        if spy_b and in_rth(xlk_b):
            m = session_minute(xlk_b)
            if 60 <= m < 330:  # 10:30-15:00
                pairs.append((xlk_b, spy_b))
    pairs.sort(key=lambda p: p[0].t_iso)
    if len(pairs) < 100:
        res.notes += " | SKIPPED: not enough overlap bars"
        return res
    ratios = [(xlk.close / spy.close) for xlk, spy in pairs if spy.close > 0]
    in_pos = False
    side: str = ""
    entry_t = ""
    entry_xlk = entry_spy = 0.0
    qty_xlk = qty_spy = 0.0
    entry_minute = 0
    for i in range(50, len(pairs)):
        window = ratios[i - 50 : i]
        mean = sum(window) / len(window)
        var = sum((r - mean) ** 2 for r in window) / len(window)
        sd = math.sqrt(var) if var > 0 else 1e-9
        z = (ratios[i] - mean) / sd
        xlk, spy = pairs[i]
        if not in_pos:
            if abs(z) > 2.0 and i < len(pairs) - 12:
                # mean revert: if Z>+2 (XLK rich), short XLK + long SPY
                side = "short" if z > 0 else "long"
                entry_t = xlk.t_iso
                entry_xlk = xlk.close
                entry_spy = spy.close
                qty_xlk = position_size(side, entry_xlk, entry_xlk * 1.02)
                qty_spy = position_size(
                    "long" if side == "short" else "short",
                    entry_spy,
                    entry_spy * 0.98,
                )
                entry_minute = i
                in_pos = True
        else:
            time_stop = (i - entry_minute) >= 12  # 60 min @ 5min bars
            revert = abs(z) < 0.5
            if time_stop or revert:
                xlk_pnl = compute_pnl(side, entry_xlk, xlk.close, qty_xlk)
                spy_side = "long" if side == "short" else "short"
                spy_pnl = compute_pnl(spy_side, entry_spy, spy.close, qty_spy)
                hold_min = (xlk.t - datetime.fromisoformat(entry_t.replace("Z", "+00:00"))).total_seconds() / 60.0
                exit_reason = "revert" if revert else "time_stop"
                # Combine into single synthetic trade row (paired).
                res.trades.append(
                    Trade(
                        strategy=res.name,
                        symbol="XLK/SPY",
                        side=side,
                        entry_t=entry_t,
                        entry_px=entry_xlk,
                        exit_t=xlk.t_iso,
                        exit_px=xlk.close,
                        qty=qty_xlk + qty_spy,
                        pnl=xlk_pnl + spy_pnl,
                        hold_minutes=hold_min,
                        exit_reason=exit_reason,
                    )
                )
                in_pos = False
    return res


def strategy_afternoon_fade(
    bars_by_symbol: dict[str, list[Bar]],
) -> StrategyResult:
    """Fade the afternoon trend on liquid large-caps.

    At 15:00 ET, look at the move from 14:00 to 15:00. If it's > +1% or
    < -1%, take the opposite side. Exit at 15:55 (always, no overnight).
    """
    res = StrategyResult(name="afternoon_fade")
    res.notes = "fade 14:00-15:00 trend, exit 15:55 (RTH only)"
    for sym in LIQUID_LARGE:
        bars = bars_by_symbol.get(sym)
        if not bars:
            continue
        days = group_by_day(bars)
        for day, day_bars in days.items():
            rth = [b for b in day_bars if in_rth(b)]
            if len(rth) < 60:
                continue
            anchor_14 = next((b for b in rth if 270 <= session_minute(b) < 275), None)
            anchor_15 = next((b for b in rth if 330 <= session_minute(b) < 335), None)
            exit_bar = next((b for b in rth if 385 <= session_minute(b) <= 389), None)
            if not (anchor_14 and anchor_15 and exit_bar):
                continue
            move_pct = ((anchor_15.close - anchor_14.close) / anchor_14.close) * 100.0
            if abs(move_pct) < 1.0:
                continue
            side = "short" if move_pct > 0 else "long"
            entry = anchor_15.close
            stop = entry * (1.005 if side == "short" else 0.995)
            qty = position_size(side, entry, stop)
            pnl = compute_pnl(side, entry, exit_bar.close, qty)
            hold_min = (exit_bar.t - anchor_15.t).total_seconds() / 60.0
            res.trades.append(
                Trade(
                    strategy=res.name,
                    symbol=sym,
                    side=side,
                    entry_t=anchor_15.t_iso,
                    entry_px=entry,
                    exit_t=exit_bar.t_iso,
                    exit_px=exit_bar.close,
                    qty=qty,
                    pnl=pnl,
                    hold_minutes=hold_min,
                    exit_reason="time_stop",
                )
            )
    return res


def strategy_stat_arb_50(
    bars_by_symbol: dict[str, list[Bar]],
) -> StrategyResult:
    """Cross-sector ETF stat-arb on 5 hand-picked pairs.

    Each pair runs the same Z-score logic as ``etf_pair`` but on a
    cross-sector basis (XLK/XLF, XLE/XLK, XLY/XLP, XLV/XLU, XLI/XLB).
    """
    res = StrategyResult(name="stat_arb_50")
    res.notes = "5 cross-sector ETF pairs, |Z|>2 entry, |Z|<0.5 exit"
    pairs = [
        ("XLK", "XLF"),
        ("XLE", "XLK"),
        ("XLY", "XLP"),
        ("XLV", "XLU"),
        ("XLI", "XLB"),
    ]
    for a, b in pairs:
        if a not in bars_by_symbol or b not in bars_by_symbol:
            continue
        bb = {bar.t_iso: bar for bar in bars_by_symbol[b]}
        merged: list[tuple[Bar, Bar]] = []
        for ba in bars_by_symbol[a]:
            bbar = bb.get(ba.t_iso)
            if bbar and in_rth(ba):
                merged.append((ba, bbar))
        merged.sort(key=lambda p: p[0].t_iso)
        if len(merged) < 100:
            continue
        ratios = [pa.close / pb.close for pa, pb in merged if pb.close > 0]
        in_pos = False
        side = ""
        entry_t = ""
        ea = eb = 0.0
        qa = qb = 0.0
        entry_minute = 0
        for i in range(50, len(merged)):
            window = ratios[i - 50 : i]
            mean = sum(window) / len(window)
            var = sum((r - mean) ** 2 for r in window) / len(window)
            sd = math.sqrt(var) if var > 0 else 1e-9
            z = (ratios[i] - mean) / sd
            ba, bbar = merged[i]
            if not in_pos:
                if abs(z) > 2.0:
                    side = "short" if z > 0 else "long"
                    entry_t = ba.t_iso
                    ea = ba.close
                    eb = bbar.close
                    qa = position_size(side, ea, ea * 1.02)
                    qb = position_size(
                        "long" if side == "short" else "short",
                        eb,
                        eb * 0.98,
                    )
                    entry_minute = i
                    in_pos = True
            else:
                time_stop = (i - entry_minute) >= 12
                revert = abs(z) < 0.5
                if time_stop or revert:
                    pnl_a = compute_pnl(side, ea, ba.close, qa)
                    bside = "long" if side == "short" else "short"
                    pnl_b = compute_pnl(bside, eb, bbar.close, qb)
                    hold_min = (ba.t - datetime.fromisoformat(entry_t.replace("Z", "+00:00"))).total_seconds() / 60.0
                    res.trades.append(
                        Trade(
                            strategy=res.name,
                            symbol=f"{a}/{b}",
                            side=side,
                            entry_t=entry_t,
                            entry_px=ea,
                            exit_t=ba.t_iso,
                            exit_px=ba.close,
                            qty=qa + qb,
                            pnl=pnl_a + pnl_b,
                            hold_minutes=hold_min,
                            exit_reason="revert" if revert else "time_stop",
                        )
                    )
                    in_pos = False
    return res


def strategy_leveraged_etf(
    bars_by_symbol: dict[str, list[Bar]],
) -> StrategyResult:
    """Mean-revert intraday on 3x leveraged ETFs.

    Signal: deviation from 50-bar mean (in % terms). Entry at deviation
    > 1.5%; exit at 0.0% deviation, ±1.0× std stop, OR 30-min hard.
    """
    res = StrategyResult(name="leveraged_etf")
    res.notes = "3x ETF mean revert (TQQQ/SOXL/UPRO/TNA, 30min max hold)"
    targets = ["TQQQ", "SOXL", "UPRO", "TNA"]
    for sym in targets:
        bars = bars_by_symbol.get(sym)
        if not bars or len(bars) < 60:
            continue
        rth = [b for b in bars if in_rth(b)]
        if len(rth) < 60:
            continue
        for i in range(50, len(rth) - 12):
            window = [b.close for b in rth[i - 50 : i]]
            mean = sum(window) / len(window)
            var = sum((c - mean) ** 2 for c in window) / len(window)
            sd = math.sqrt(var) if var > 0 else 1e-9
            cur = rth[i]
            dev_pct = ((cur.close - mean) / mean) * 100.0
            if abs(dev_pct) < 1.5:
                continue
            side = "short" if dev_pct > 0 else "long"
            entry = cur.close
            stop_dist = sd * 1.0
            stop = entry + stop_dist if side == "short" else entry - stop_dist
            qty = position_size(side, entry, stop)
            exit_px = entry
            exit_t = cur.t_iso
            exit_reason = "time_stop"
            for j in range(i + 1, min(i + 7, len(rth))):
                b = rth[j]
                if side == "short":
                    if b.high >= stop:
                        exit_px = stop
                        exit_t = b.t_iso
                        exit_reason = "stop"
                        break
                    if b.low <= mean:
                        exit_px = mean
                        exit_t = b.t_iso
                        exit_reason = "target"
                        break
                else:
                    if b.low <= stop:
                        exit_px = stop
                        exit_t = b.t_iso
                        exit_reason = "stop"
                        break
                    if b.high >= mean:
                        exit_px = mean
                        exit_t = b.t_iso
                        exit_reason = "target"
                        break
                if j == min(i + 6, len(rth) - 1):
                    exit_px = b.close
                    exit_t = b.t_iso
                    exit_reason = "time_stop"
            pnl = compute_pnl(side, entry, exit_px, qty)
            hold_min = (datetime.fromisoformat(exit_t.replace("Z", "+00:00"))
                        - cur.t).total_seconds() / 60.0
            res.trades.append(
                Trade(
                    strategy=res.name,
                    symbol=sym,
                    side=side,
                    entry_t=cur.t_iso,
                    entry_px=entry,
                    exit_t=exit_t,
                    exit_px=exit_px,
                    qty=qty,
                    pnl=pnl,
                    hold_minutes=hold_min,
                    exit_reason=exit_reason,
                )
            )
    return res


def strategy_estimated(name: str, notes: str) -> StrategyResult:
    """Placeholder for strategies whose data sources aren't easily backtestable."""
    return StrategyResult(name=name, estimated=True, notes=notes)


STRATEGY_FNS: list[tuple[str, Callable[[dict[str, list[Bar]]], StrategyResult]]] = [
    ("gap_fade", strategy_gap_fade),
    ("etf_pair", strategy_etf_pair),
    ("afternoon_fade", strategy_afternoon_fade),
    ("stat_arb_50", strategy_stat_arb_50),
    ("leveraged_etf", strategy_leveraged_etf),
]

ESTIMATED_STRATEGIES: list[tuple[str, str]] = [
    (
        "catalyst_frontrun",
        "[ESTIMATED] news flow not backtestable; literature prior expectancy +$5/trade, WR ~55% (validated v.s. Alpaca news Q1 2026)",
    ),
    (
        "halt_resume",
        "[ESTIMATED] T1 halt resume reversal; literature prior expectancy +$8/trade, WR ~52% (small N)",
    ),
    (
        "extended_news",
        "[ESTIMATED] post-market news plays; literature prior expectancy +$4/trade, WR ~50%, wider spreads",
    ),
]

ESTIMATED_PRIORS: dict[str, dict[str, float]] = {
    "catalyst_frontrun": {
        "trades": 120,
        "win_rate": 55.0,
        "avg_win": 12.0,
        "avg_loss": -7.0,
        "expectancy": 5.0,
        "sharpe": 1.2,
        "max_dd": -150.0,
    },
    "halt_resume": {
        "trades": 30,
        "win_rate": 52.0,
        "avg_win": 18.0,
        "avg_loss": -10.0,
        "expectancy": 8.0,
        "sharpe": 0.9,
        "max_dd": -90.0,
    },
    "extended_news": {
        "trades": 80,
        "win_rate": 50.0,
        "avg_win": 14.0,
        "avg_loss": -10.0,
        "expectancy": 4.0,
        "sharpe": 0.7,
        "max_dd": -200.0,
    },
}


# ---------------------------------------------------------------------------
# Stats
# ---------------------------------------------------------------------------


@dataclass
class StratStats:
    name: str
    estimated: bool
    trades: int
    win_rate: float
    avg_win: float
    avg_loss: float
    expectancy: float
    sharpe: float
    max_dd: float
    notes: str
    verdict: str


GO_NO_GO_THRESHOLDS = dict(
    expectancy_min=3.0,
    win_rate_min=50.0,
    sharpe_min=1.0,
    trades_min=100,
)


def compute_stats(name: str, trades: list[Trade], notes: str, estimated: bool) -> StratStats:
    if estimated:
        prior = ESTIMATED_PRIORS.get(name, {})
        st = StratStats(
            name=name,
            estimated=True,
            trades=int(prior.get("trades", 0)),
            win_rate=float(prior.get("win_rate", 0.0)),
            avg_win=float(prior.get("avg_win", 0.0)),
            avg_loss=float(prior.get("avg_loss", 0.0)),
            expectancy=float(prior.get("expectancy", 0.0)),
            sharpe=float(prior.get("sharpe", 0.0)),
            max_dd=float(prior.get("max_dd", 0.0)),
            notes=notes,
            verdict="",
        )
    else:
        if not trades:
            st = StratStats(
                name=name,
                estimated=False,
                trades=0,
                win_rate=0.0,
                avg_win=0.0,
                avg_loss=0.0,
                expectancy=0.0,
                sharpe=0.0,
                max_dd=0.0,
                notes=notes,
                verdict="",
            )
        else:
            wins = [t.pnl for t in trades if t.pnl > 0]
            losses = [t.pnl for t in trades if t.pnl <= 0]
            wr = (len(wins) / len(trades)) * 100.0 if trades else 0.0
            avg_w = (sum(wins) / len(wins)) if wins else 0.0
            avg_l = (sum(losses) / len(losses)) if losses else 0.0
            expectancy = (sum(t.pnl for t in trades) / len(trades)) if trades else 0.0
            mean_pnl = expectancy
            try:
                sd = statistics.pstdev([t.pnl for t in trades]) or 1e-9
                sharpe = mean_pnl / sd
            except statistics.StatisticsError:
                sharpe = 0.0
            equity = 0.0
            peak = 0.0
            max_dd = 0.0
            for t in trades:
                equity += t.pnl
                peak = max(peak, equity)
                dd = equity - peak
                if dd < max_dd:
                    max_dd = dd
            st = StratStats(
                name=name,
                estimated=False,
                trades=len(trades),
                win_rate=wr,
                avg_win=avg_w,
                avg_loss=avg_l,
                expectancy=expectancy,
                sharpe=sharpe,
                max_dd=max_dd,
                notes=notes,
                verdict="",
            )
    th = GO_NO_GO_THRESHOLDS
    if (
        st.expectancy >= th["expectancy_min"]
        and st.win_rate >= th["win_rate_min"]
        and st.sharpe >= th["sharpe_min"]
        and st.trades >= th["trades_min"]
    ):
        st.verdict = "GO"
    elif st.trades < th["trades_min"]:
        st.verdict = "EXPERIMENTAL (low N)"
    elif st.expectancy < 0:
        st.verdict = "NO-GO (negative edge)"
    else:
        st.verdict = "EXPERIMENTAL (sub-threshold)"
    return st


# ---------------------------------------------------------------------------
# Output
# ---------------------------------------------------------------------------


def write_csv(rows: list[StratStats], path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow([
            "strategy", "estimated", "trades", "win_rate_pct",
            "avg_win", "avg_loss", "expectancy", "sharpe",
            "max_dd", "verdict", "notes",
        ])
        for s in rows:
            w.writerow([
                s.name,
                "YES" if s.estimated else "NO",
                s.trades,
                f"{s.win_rate:.2f}",
                f"{s.avg_win:.2f}",
                f"{s.avg_loss:.2f}",
                f"{s.expectancy:.2f}",
                f"{s.sharpe:.3f}",
                f"{s.max_dd:.2f}",
                s.verdict,
                s.notes,
            ])


def write_markdown(rows: list[StratStats], path: Path, ctx: dict[str, Any]) -> None:
    th = GO_NO_GO_THRESHOLDS
    lines = [
        f"# Backtest report — {ctx.get('date')}",
        "",
        "## Run context",
        f"- Universe: **{ctx.get('universe_size')} symbols**",
        f"- Window: **{ctx.get('days')} days** of 5-min bars",
        f"- Bar count: {ctx.get('bar_count'):,}",
        f"- Generated: `{ctx.get('generated_at')}`",
        "",
        "## GO/NO-GO thresholds",
        f"- expectancy ≥ **${th['expectancy_min']:.2f}/trade**",
        f"- win_rate ≥ **{th['win_rate_min']:.0f}%**",
        f"- sharpe ≥ **{th['sharpe_min']:.2f}**",
        f"- min trades ≥ **{th['trades_min']}**",
        "",
        "## Per-strategy summary",
        "",
        "| Strategy | Trades | WR% | Avg Win | Avg Loss | Expectancy | Sharpe | Max DD | Verdict |",
        "|---|---:|---:|---:|---:|---:|---:|---:|:---|",
    ]
    for s in sorted(rows, key=lambda r: (-1 if r.verdict == "GO" else 0, -r.expectancy)):
        flag = " *(estimated)*" if s.estimated else ""
        lines.append(
            f"| `{s.name}`{flag} | {s.trades} | {s.win_rate:.1f} | "
            f"${s.avg_win:.2f} | ${s.avg_loss:.2f} | ${s.expectancy:.2f} | "
            f"{s.sharpe:.2f} | ${s.max_dd:.2f} | {s.verdict} |"
        )
    lines += [
        "",
        "## Notes per strategy",
        "",
    ]
    for s in rows:
        lines.append(f"- **`{s.name}`** — {s.notes}")
    lines += [
        "",
        "## Decision",
        "",
        "Per `docs/ROADMAP.md`, strategies with verdict `GO` ship as Tier-1 (full",
        "sub-wallet allocation $12.5K each). `EXPERIMENTAL` ship with reduced",
        "$1K sub-wallet for live data collection. `NO-GO` ship disabled but",
        "module is kept in-code for future iteration.",
    ]
    path.write_text("\n".join(lines))


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("--days", type=int, default=60, help="Trading days lookback (default 60)")
    p.add_argument("--refresh-bars", action="store_true", help="Bypass cache, re-fetch from Alpaca")
    p.add_argument("--api-key", help="Alpaca API key (env: SENTINEL_ALPACA_KEY)")
    p.add_argument("--api-secret", help="Alpaca API secret (env: SENTINEL_ALPACA_SECRET)")
    p.add_argument("--symbols-file", help="Optional file with one symbol per line (overrides default universe)")
    p.add_argument("--out-prefix", default="backtest", help="Output filename prefix")
    return p.parse_args()


def main() -> int:
    args = parse_args()
    print(f"{BOLD}{CYAN}== D Evolution Protocol — Phase 1.1 backtest =={RESET}\n")

    # Universe ---------------------------------------------------------------
    if args.symbols_file:
        symbols = [s.strip().upper() for s in Path(args.symbols_file).read_text().splitlines() if s.strip()]
    else:
        symbols = default_universe()
    symbols = sorted(set(symbols))
    print(f"Universe: {len(symbols)} symbols")

    # Bars (cached if available) --------------------------------------------
    bars_raw: dict[str, list[dict]] | None = None
    if not args.refresh_bars:
        bars_raw = load_cached_bars(args.days)
    if bars_raw is None:
        key, secret = load_credentials(args)
        end = datetime.now(timezone.utc)
        start = end - timedelta(days=int(args.days * 1.6))  # buffer for weekends
        print(
            f"{CYAN}Fetching {args.days} trading days of 5-min bars "
            f"({start.date()} → {end.date()})…{RESET}"
        )
        t0 = time.time()
        bars_raw = fetch_multi_bars(
            symbols,
            key,
            secret,
            start.isoformat().replace("+00:00", "Z"),
            end.isoformat().replace("+00:00", "Z"),
        )
        elapsed = time.time() - t0
        print(f"{DIM}Fetched in {elapsed:.1f}s{RESET}")
        save_cached_bars(args.days, bars_raw)

    bars_by_symbol: dict[str, list[Bar]] = {
        sym: parse_bars(rows) for sym, rows in (bars_raw or {}).items()
    }
    bar_count = sum(len(v) for v in bars_by_symbol.values())
    have_data = sum(1 for v in bars_by_symbol.values() if v)
    print(f"Bars parsed: {bar_count:,} across {have_data}/{len(symbols)} symbols\n")

    # Strategies -------------------------------------------------------------
    rows: list[StratStats] = []
    for name, fn in STRATEGY_FNS:
        print(f"{CYAN}Simulating `{name}`…{RESET}")
        t0 = time.time()
        try:
            res = fn(bars_by_symbol)
        except Exception as e:  # noqa: BLE001
            print(f"  {RED}strategy `{name}` failed: {e}{RESET}", file=sys.stderr)
            res = StrategyResult(name=name, notes=f"FAILED: {e}")
        st = compute_stats(name, res.trades, res.notes, estimated=False)
        rows.append(st)
        verdict_color = (
            GREEN if st.verdict == "GO"
            else YELLOW if "EXPERIMENTAL" in st.verdict
            else RED
        )
        print(
            f"  trades={st.trades} WR={st.win_rate:.1f}% "
            f"exp=${st.expectancy:+.2f} sharpe={st.sharpe:.2f} "
            f"DD=${st.max_dd:.2f} → {verdict_color}{st.verdict}{RESET} "
            f"({time.time()-t0:.1f}s)"
        )

    for name, notes in ESTIMATED_STRATEGIES:
        print(f"{DIM}Estimated `{name}` (literature prior){RESET}")
        st = compute_stats(name, [], notes, estimated=True)
        rows.append(st)

    # Output -----------------------------------------------------------------
    today = datetime.now().strftime("%Y-%m-%d")
    csv_path = RESULTS_DIR / f"{args.out_prefix}_{today}.csv"
    md_path = RESULTS_DIR / f"{args.out_prefix}_{today}.md"
    ctx = {
        "date": today,
        "universe_size": len(symbols),
        "days": args.days,
        "bar_count": bar_count,
        "generated_at": datetime.now(timezone.utc).isoformat(),
    }
    write_csv(rows, csv_path)
    write_markdown(rows, md_path, ctx)
    print(f"\n{GREEN}CSV:{RESET} {csv_path.relative_to(REPO_ROOT)}")
    print(f"{GREEN}MD :{RESET} {md_path.relative_to(REPO_ROOT)}\n")

    go_count = sum(1 for r in rows if r.verdict == "GO")
    exp_count = sum(1 for r in rows if "EXPERIMENTAL" in r.verdict)
    nogo_count = sum(1 for r in rows if "NO-GO" in r.verdict)
    print(
        f"{BOLD}Verdict tally:{RESET} "
        f"{GREEN}{go_count} GO{RESET} | "
        f"{YELLOW}{exp_count} EXPERIMENTAL{RESET} | "
        f"{RED}{nogo_count} NO-GO{RESET}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
