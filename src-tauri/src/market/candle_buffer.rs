use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use chrono::TimeZone;
use titan_types::Candle;

pub const MAX_CANDLES: usize = 200;

pub fn timeframe_to_secs(tf: &str) -> i64 {
    match tf {
        "1m" | "1Min" => 60,
        "5m" | "5Min" => 300,
        "10m" | "10Min" => 600,
        "15m" | "15Min" => 900,
        "30m" | "30Min" => 1800,
        "1h" | "1Hour" => 3600,
        "4h" | "4Hour" => 14400,
        "1d" | "1D" | "1Day" => 86400,
        _ => 60,
    }
}

/// In-memory ring buffer of candles for a single symbol+timeframe pair.
///
/// The streamer feeds trade ticks into `update_trade`, which maintains a
/// "current" candle being built and a deque of completed historical candles.
/// Returns `true` from `update_trade` when a candle is finalized (bar close).
pub struct CandleRingBuffer {
    candles: VecDeque<Candle>,
    current: Option<Candle>,
    tf_secs: i64,
}

impl CandleRingBuffer {
    pub fn new(tf_secs: i64) -> Self {
        Self {
            candles: VecDeque::with_capacity(MAX_CANDLES + 1),
            current: None,
            tf_secs: tf_secs.max(1),
        }
    }

    /// Populate the buffer with historical candles from a REST fetch.
    /// Replaces any existing content. Keeps at most `MAX_CANDLES` candles.
    #[allow(dead_code)] // Used by chart data load (Session 4)
    pub fn seed(&mut self, mut bars: Vec<Candle>) {
        bars.sort_by_key(|c| c.time);
        self.candles.clear();
        self.current = None;

        let skip = bars.len().saturating_sub(MAX_CANDLES);
        for c in bars.into_iter().skip(skip) {
            self.candles.push_back(c);
        }
    }

    /// Compute the bucket start for a 1D candle anchored at 09:30 ET (DST-aware).
    fn daily_bucket_start(trade_ts: i64) -> i64 {
        let Some(utc_dt) = chrono::DateTime::<chrono::Utc>::from_timestamp(trade_ts, 0) else {
            return 0;
        };
        let trade_et = utc_dt.with_timezone(&chrono_tz::US::Eastern);
        let session_anchor = trade_et.date_naive().and_hms_opt(9, 30, 0).unwrap();
        let mut anchor_dt = chrono_tz::US::Eastern
            .from_local_datetime(&session_anchor)
            .single()
            .unwrap();
        if trade_et.naive_local() < session_anchor {
            anchor_dt -= chrono::Duration::days(1);
        }
        anchor_dt.with_timezone(&chrono::Utc).timestamp()
    }

    /// Feed a trade tick. Returns `true` when a candle was finalized (bar close).
    pub fn update_trade(&mut self, price: f64, size: f64, trade_ts: i64) -> bool {
        if price <= 0.0 {
            return false;
        }
        let bucket_start = if self.tf_secs == 86400 {
            Self::daily_bucket_start(trade_ts)
        } else {
            (trade_ts / self.tf_secs) * self.tf_secs
        };

        match self.current.as_mut() {
            Some(cur) if cur.time == bucket_start => {
                cur.high = cur.high.max(price);
                cur.low = cur.low.min(price);
                cur.close = price;
                cur.volume += size;
                false
            }
            Some(_) => {
                let finished = self.current.take().unwrap();
                self.push_finished(finished);
                self.current = Some(Candle {
                    time: bucket_start,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume: size,
                });
                true
            }
            None => {
                self.current = Some(Candle {
                    time: bucket_start,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume: size,
                });
                false
            }
        }
    }

    /// Returns completed candles plus the current in-progress candle (if any).
    #[allow(dead_code)] // Used by indicator cache + chart (Sessions 4-6)
    pub fn snapshot(&self) -> Vec<Candle> {
        let mut out = Vec::with_capacity(self.candles.len() + 1);
        out.extend(self.candles.iter().cloned());
        if let Some(cur) = &self.current {
            out.push(cur.clone());
        }
        out
    }

    /// Most recently finalized candle (not the in-progress one).
    pub fn last_closed(&self) -> Option<&Candle> {
        self.candles.back()
    }

    #[allow(dead_code)] // Used by sentinel for min-bars check (Session 6)
    pub fn len(&self) -> usize {
        self.candles.len()
    }

    #[allow(dead_code)] // Used by sentinel (Session 6)
    pub fn available(&self) -> usize {
        self.candles.len() + usize::from(self.current.is_some())
    }

    /// Push a single finished candle (used by history replay).
    pub fn push_candle(&mut self, candle: Candle) {
        self.push_finished(candle);
    }

    fn push_finished(&mut self, candle: Candle) {
        if self.candles.len() >= MAX_CANDLES {
            self.candles.pop_front();
        }
        self.candles.push_back(candle);
    }
}

// ═══════════════════════════════════════════════════════════════════
//  FLASH CRASH DETECTOR — rolling price window with auto-protection
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize)]
pub struct FlashCrashEvent {
    pub symbol: String,
    pub change_pct: f64,
    pub window_secs: u64,
}

const MAX_TRACKED_SYMBOLS: usize = 500;

pub struct FlashCrashDetector {
    windows: HashMap<String, VecDeque<(Instant, f64)>>,
    blocked_until: HashMap<String, Instant>,
    last_seen: HashMap<String, Instant>,
    window_duration: Duration,
    threshold_pct: f64,
    cooldown: Duration,
}

impl FlashCrashDetector {
    pub fn new(window_secs: u64, threshold_pct: f64, cooldown_mins: u64) -> Self {
        Self {
            windows: HashMap::new(),
            blocked_until: HashMap::new(),
            last_seen: HashMap::new(),
            window_duration: Duration::from_secs(window_secs),
            threshold_pct,
            cooldown: Duration::from_secs(cooldown_mins * 60),
        }
    }

    pub fn check_trade(&mut self, symbol: &str, price: f64, now: Instant) -> Option<FlashCrashEvent> {
        if price <= 0.0 {
            return None;
        }

        if let Some(&blocked) = self.blocked_until.get(symbol) {
            if now < blocked {
                return None;
            }
            self.blocked_until.remove(symbol);
        }

        self.last_seen.insert(symbol.to_string(), now);

        let result = {
            let window = self.windows.entry(symbol.to_string()).or_default();
            window.push_back((now, price));

            while let Some(&(ts, _)) = window.front() {
                if now.duration_since(ts) > self.window_duration {
                    window.pop_front();
                } else {
                    break;
                }
            }

            if window.len() < 2 {
                None
            } else {
                let oldest_price = window.front().unwrap().1;
                let change_pct = ((price - oldest_price) / oldest_price) * 100.0;

                if change_pct.abs() > self.threshold_pct {
                    self.blocked_until
                        .insert(symbol.to_string(), now + self.cooldown);
                    Some(FlashCrashEvent {
                        symbol: symbol.to_string(),
                        change_pct,
                        window_secs: self.window_duration.as_secs(),
                    })
                } else {
                    None
                }
            }
        };

        self.evict_if_over_cap();
        result
    }

    #[allow(dead_code)] // Used by bot safety checks (Session 9)
    pub fn is_blocked(&self, symbol: &str) -> bool {
        match self.blocked_until.get(symbol) {
            Some(&until) => Instant::now() < until,
            None => false,
        }
    }

    #[allow(dead_code)] // Used from sentinel/risk (Sessions 6+, 11+)
    pub fn reconfigure(&mut self, window_secs: u64, threshold_pct: f64, cooldown_mins: u64) {
        self.window_duration = Duration::from_secs(window_secs);
        self.threshold_pct = threshold_pct;
        self.cooldown = Duration::from_secs(cooldown_mins * 60);
    }

    fn evict_if_over_cap(&mut self) {
        while self.windows.len() > MAX_TRACKED_SYMBOLS {
            let oldest = self
                .last_seen
                .iter()
                .min_by_key(|(_, &ts)| ts)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest {
                self.windows.remove(&key);
                self.blocked_until.remove(&key);
                self.last_seen.remove(&key);
            } else {
                break;
            }
        }
    }

    #[allow(dead_code)] // Used on symbol change/reconnect
    pub fn clear(&mut self) {
        self.windows.clear();
        self.blocked_until.clear();
        self.last_seen.clear();
    }

    #[cfg(test)]
    pub fn tracked_symbols(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_buf(tf: &str) -> CandleRingBuffer {
        CandleRingBuffer::new(timeframe_to_secs(tf))
    }

    #[test]
    fn seed_limits_to_max_candles() {
        let mut buf = make_buf("1m");
        let bars: Vec<Candle> = (0..300)
            .map(|i| Candle {
                time: i * 60,
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.0,
                volume: 1.0,
            })
            .collect();
        buf.seed(bars);
        assert_eq!(buf.len(), MAX_CANDLES);
        assert_eq!(buf.candles.front().unwrap().time, 100 * 60);
    }

    #[test]
    fn update_trade_builds_ohlcv_within_same_candle() {
        let mut buf = make_buf("1m");
        let ts = 120;
        assert!(!buf.update_trade(100.0, 10.0, ts));
        assert!(!buf.update_trade(105.0, 5.0, ts + 10));
        assert!(!buf.update_trade(98.0, 3.0, ts + 20));
        assert!(!buf.update_trade(102.0, 7.0, ts + 50));

        let snap = buf.snapshot();
        assert_eq!(snap.len(), 1);
        let c = &snap[0];
        assert_eq!(c.time, 120);
        assert_eq!(c.open, 100.0);
        assert_eq!(c.high, 105.0);
        assert_eq!(c.low, 98.0);
        assert_eq!(c.close, 102.0);
        assert!((c.volume - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn update_trade_finalizes_on_boundary_cross() {
        let mut buf = make_buf("1m");
        assert!(!buf.update_trade(100.0, 10.0, 60));
        assert!(!buf.update_trade(101.0, 5.0, 80));
        assert!(buf.update_trade(102.0, 8.0, 120));

        assert_eq!(buf.len(), 1);
        assert_eq!(buf.available(), 2);
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].time, 60);
        assert_eq!(snap[0].close, 101.0);
        assert_eq!(snap[1].time, 120);
        assert_eq!(snap[1].open, 102.0);
    }

    #[test]
    fn ring_evicts_oldest_when_full() {
        let mut buf = make_buf("1m");
        for i in 0..(MAX_CANDLES as i64 + 5) {
            let ts = i * 60;
            buf.update_trade(100.0, 1.0, ts);
            buf.update_trade(100.0, 1.0, ts + 60);
        }
        assert!(buf.len() <= MAX_CANDLES);
    }

    #[test]
    fn snapshot_after_seed_and_update_is_sorted() {
        let mut buf = make_buf("5m");
        let seed_bars: Vec<Candle> = (0..10)
            .map(|i| Candle {
                time: i * 300,
                open: 50.0,
                high: 51.0,
                low: 49.0,
                close: 50.0,
                volume: 100.0,
            })
            .collect();
        buf.seed(seed_bars);
        buf.update_trade(55.0, 20.0, 10 * 300 + 10);

        let snap = buf.snapshot();
        assert_eq!(snap.len(), 11);
        for w in snap.windows(2) {
            assert!(w[0].time <= w[1].time);
        }
    }

    #[test]
    fn timeframe_conversion() {
        assert_eq!(timeframe_to_secs("1m"), 60);
        assert_eq!(timeframe_to_secs("5Min"), 300);
        assert_eq!(timeframe_to_secs("1h"), 3600);
        assert_eq!(timeframe_to_secs("1Hour"), 3600);
        assert_eq!(timeframe_to_secs("1d"), 86400);
        assert_eq!(timeframe_to_secs("unknown"), 60);
    }

    #[test]
    fn ignores_zero_or_negative_prices() {
        let mut buf = make_buf("1m");
        buf.update_trade(0.0, 1.0, 60);
        buf.update_trade(-5.0, 1.0, 60);
        assert_eq!(buf.available(), 0);
    }

    // ── Flash Crash Detector tests ──

    #[test]
    fn flash_crash_no_event_on_normal_trades() {
        let mut det = FlashCrashDetector::new(30, 3.0, 5);
        let t0 = Instant::now();
        for i in 0..10 {
            let price = 100.0 + (i as f64) * 0.1;
            let result = det.check_trade("AAPL", price, t0 + Duration::from_secs(i));
            assert!(result.is_none());
        }
    }

    #[test]
    fn flash_crash_detects_crash() {
        let mut det = FlashCrashDetector::new(30, 3.0, 5);
        let t0 = Instant::now();
        det.check_trade("AAPL", 100.0, t0);
        let event = det.check_trade("AAPL", 96.0, t0 + Duration::from_secs(10));
        assert!(event.is_some());
        let ev = event.unwrap();
        assert_eq!(ev.symbol, "AAPL");
        assert!(ev.change_pct < -3.0);
    }

    #[test]
    fn flash_crash_detects_spike_up() {
        let mut det = FlashCrashDetector::new(30, 3.0, 5);
        let t0 = Instant::now();
        det.check_trade("TSLA", 200.0, t0);
        let event = det.check_trade("TSLA", 208.0, t0 + Duration::from_secs(5));
        assert!(event.is_some());
        assert!(event.unwrap().change_pct > 3.0);
    }

    #[test]
    fn flash_crash_cooldown_blocks() {
        let mut det = FlashCrashDetector::new(30, 3.0, 5);
        let t0 = Instant::now();
        det.check_trade("AAPL", 100.0, t0);
        det.check_trade("AAPL", 96.0, t0 + Duration::from_secs(5));

        assert!(det.is_blocked("AAPL"));
        assert!(!det.is_blocked("TSLA"));

        let event = det.check_trade("AAPL", 50.0, t0 + Duration::from_secs(10));
        assert!(event.is_none());
    }

    #[test]
    fn flash_crash_clear_resets_state() {
        let mut det = FlashCrashDetector::new(30, 3.0, 5);
        let t0 = Instant::now();
        det.check_trade("AAPL", 100.0, t0);
        det.clear();
        let event = det.check_trade("AAPL", 50.0, t0 + Duration::from_secs(1));
        assert!(event.is_none());
    }

    // ── 1D bucket session-aware ET tests ──

    #[test]
    fn update_trade_1d_anchors_to_eastern_session_open_summer() {
        // 2026-06-15 14:00 UTC = 10:00 ET (EDT, UTC-4)
        // 09:30 ET in summer = 13:30 UTC
        let trade_ts = chrono::NaiveDate::from_ymd_opt(2026, 6, 15)
            .unwrap()
            .and_hms_opt(14, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 6, 15)
            .unwrap()
            .and_hms_opt(13, 30, 0)
            .unwrap()
            .and_utc()
            .timestamp();

        let mut buf = CandleRingBuffer::new(86400);
        buf.update_trade(100.0, 10.0, trade_ts);
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].time, expected);
    }

    #[test]
    fn update_trade_1d_pre_open_belongs_to_previous_session() {
        // 2026-06-15 12:00 UTC = 08:00 ET (before 09:30 open)
        // Should anchor to previous day's 09:30 ET = 2026-06-14 13:30 UTC
        let trade_ts = chrono::NaiveDate::from_ymd_opt(2026, 6, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 6, 14)
            .unwrap()
            .and_hms_opt(13, 30, 0)
            .unwrap()
            .and_utc()
            .timestamp();

        let mut buf = CandleRingBuffer::new(86400);
        buf.update_trade(100.0, 10.0, trade_ts);
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].time, expected);
    }

    #[test]
    fn update_trade_1d_winter_anchors_correctly() {
        // 2026-12-15 15:00 UTC = 10:00 ET (EST, UTC-5)
        // 09:30 ET in winter = 14:30 UTC
        let trade_ts = chrono::NaiveDate::from_ymd_opt(2026, 12, 15)
            .unwrap()
            .and_hms_opt(15, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp();
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 12, 15)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap()
            .and_utc()
            .timestamp();

        let mut buf = CandleRingBuffer::new(86400);
        buf.update_trade(100.0, 10.0, trade_ts);
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].time, expected);
    }

    // ── FlashCrashDetector LRU cap tests (S06) ──

    #[test]
    fn flash_crash_detector_evicts_when_over_cap() {
        let mut det = FlashCrashDetector::new(30, 50.0, 5);
        let t0 = Instant::now();
        for i in 0..600 {
            let sym = format!("SYM{i:04}");
            det.check_trade(&sym, 100.0, t0 + Duration::from_millis(i as u64));
        }
        assert!(
            det.tracked_symbols() <= MAX_TRACKED_SYMBOLS,
            "tracked={} > cap={}",
            det.tracked_symbols(),
            MAX_TRACKED_SYMBOLS,
        );
    }

    #[test]
    fn flash_crash_detector_recent_use_keeps_entry() {
        let mut det = FlashCrashDetector::new(30, 50.0, 5);
        let t0 = Instant::now();
        det.check_trade("KEEPER", 100.0, t0);
        for i in 1..=501 {
            let sym = format!("OTHER{i:04}");
            det.check_trade(&sym, 100.0, t0 + Duration::from_millis(i as u64));
            if i % 50 == 0 {
                det.check_trade("KEEPER", 100.0, t0 + Duration::from_millis(i as u64 + 1));
            }
        }
        assert!(
            det.windows.contains_key("KEEPER"),
            "Recently-used entry must survive eviction"
        );
    }
}
