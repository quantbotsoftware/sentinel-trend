use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use titan_indicators::{
    IndicatorPeriods, Indicators, StreamingIndicators,
};
use titan_types::Candle;

use crate::market::market_state::MarketDataState;
use crate::state::TitanState;

const EVICT_INTERVAL_SECS: u64 = 300;
const EVICT_MAX_AGE_SECS: u64 = 600;
/// S15.6: chunk the 5-min sleep into 30s slices so the watchdog ticks
/// at half the `STALE_WARN_SECS=60` cadence. Mirrors the
/// `chunked_tick_sleep` pattern in `market::alpaca_news`. Eviction
/// behaviour is unchanged — runs once per `EVICT_CHUNKS_PER_CYCLE` ticks.
const EVICT_TICK_CHUNK_SECS: u64 = 30;
const EVICT_CHUNKS_PER_CYCLE: u64 = EVICT_INTERVAL_SECS / EVICT_TICK_CHUNK_SECS;

pub async fn run_indicator_cache_evictor(state: Arc<TitanState>, market: Arc<MarketDataState>) {
    tracing::info!(component = "indicator_evictor", "Indicator cache evictor started");

    {
        let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
        sys.register_loop("indicator_evictor");
    }

    let mut chunk_count: u64 = 0;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(EVICT_TICK_CHUNK_SECS)) => {}
            _ = state.shutdown.cancelled() => break,
        }

        {
            let mut sys = state.system.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = sys.loops.get_mut("indicator_evictor") {
                h.tick();
            }
        }

        chunk_count += 1;
        if !chunk_count.is_multiple_of(EVICT_CHUNKS_PER_CYCLE) {
            continue;
        }

        let evicted = {
            let mut cache = market.indicator_cache.lock().unwrap_or_else(|p| p.into_inner());
            let before = cache.len();
            cache.evict_stale(Duration::from_secs(EVICT_MAX_AGE_SECS));
            before - cache.len()
        };

        if evicted > 0 {
            tracing::debug!(component = "indicator_evictor", evicted, "Evicted stale indicator cache entries");
        }
    }

    tracing::info!(component = "indicator_evictor", "Indicator cache evictor exiting");
}

#[allow(dead_code)]
struct CacheEntry {
    streaming: StreamingIndicators,
    last_candle_ts: i64,
    snapshot: Indicators,
    last_access: Instant,
}

/// Per-symbol+timeframe indicator cache with hybrid strategy:
/// - EMA/RSI/MACD/BB/ADX/VWAP/StochRSI: O(1) incremental via StreamingIndicators
/// - Cache hit (same candle timestamp): returns stored snapshot instantly
pub struct IndicatorCache {
    entries: HashMap<String, CacheEntry>,
}

fn cache_key(symbol: &str, timeframe: &str) -> String {
    format!("{symbol}:{timeframe}")
}

impl IndicatorCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// 1. Cache hit (same ts) -> clone snapshot
    /// 2. Cache miss (new candle) -> streaming.update -> store
    /// 3. Cold start -> full batch compute
    pub fn get_or_compute(
        &mut self,
        symbol: &str,
        timeframe: &str,
        candles: &[Candle],
        periods: &IndicatorPeriods,
    ) -> Indicators {
        if candles.is_empty() {
            return empty_indicators();
        }

        let last_ts = candles[candles.len() - 1].time;
        let key = cache_key(symbol, timeframe);

        if let Some(entry) = self.entries.get_mut(&key) {
            if entry.last_candle_ts == last_ts {
                entry.last_access = Instant::now();
                return entry.snapshot.clone();
            }

            let streaming_snap = entry.streaming.update(&candles[candles.len() - 1]);
            entry.last_candle_ts = last_ts;
            entry.snapshot = streaming_snap;
            entry.last_access = Instant::now();
            return entry.snapshot.clone();
        }

        let mut streaming = StreamingIndicators::cold_start(candles, periods);
        let snapshot = streaming.update(&candles[candles.len() - 1]);

        self.entries.insert(
            key.clone(),
            CacheEntry {
                streaming,
                last_candle_ts: last_ts,
                snapshot,
                last_access: Instant::now(),
            },
        );

        self.entries
            .get(&key)
            .map(|e| e.snapshot.clone())
            .unwrap_or_else(empty_indicators)
    }

    pub fn evict_stale(&mut self, max_age: std::time::Duration) {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_access) < max_age);
    }

    #[allow(dead_code)]
    pub fn peek(&self, symbol: &str, timeframe: &str) -> Option<Indicators> {
        self.entries
            .get(&cache_key(symbol, timeframe))
            .map(|entry| entry.snapshot.clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[allow(dead_code)]
fn empty_indicators() -> Indicators {
    Indicators {
        ema_9: None,
        ema_20: None,
        ema_50: None,
        ema_200: None,
        rsi: None,
        macd: None,
        macd_signal: None,
        macd_hist: None,
        bb_upper: None,
        bb_lower: None,
        bb_mid: None,
        adx: None,
        vwap: None,
        stoch_rsi: None,
        stoch_k: None,
        stoch_d: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_candle(time: i64, close: f64) -> Candle {
        Candle {
            time,
            open: close - 0.5,
            high: close + 1.0,
            low: close - 1.0,
            close,
            volume: 1000.0,
        }
    }

    fn make_candles(n: usize, base_time: i64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let t = base_time + i as i64 * 60;
                let close = 100.0 + (i as f64) * 0.1 + ((i as f64) * 0.7).sin() * 2.0;
                make_candle(t, close)
            })
            .collect()
    }

    #[test]
    fn cold_start_populates_cache() {
        let mut cache = IndicatorCache::new();
        let candles = make_candles(200, 1000);
        let result = cache.get_or_compute(
            "AAPL",
            "5m",
            &candles,
            &IndicatorPeriods::default(),
        );
        assert_eq!(cache.len(), 1);
        assert!(result.ema_9.is_some());
        assert!(result.rsi.is_some());
    }

    #[test]
    fn cache_hit_returns_same_snapshot() {
        let mut cache = IndicatorCache::new();
        let candles = make_candles(200, 1000);
        let p = IndicatorPeriods::default();

        let first = cache.get_or_compute("AAPL", "5m", &candles, &p);
        let second = cache.get_or_compute("AAPL", "5m", &candles, &p);
        assert_eq!(first.ema_9, second.ema_9);
    }

    #[test]
    fn new_candle_updates_cache() {
        let mut cache = IndicatorCache::new();
        let mut candles = make_candles(200, 1000);
        let p = IndicatorPeriods::default();

        let first = cache.get_or_compute("AAPL", "5m", &candles, &p);
        candles.push(make_candle(1000 + 200 * 60, 115.0));
        let second = cache.get_or_compute("AAPL", "5m", &candles, &p);
        assert_ne!(first.ema_9, second.ema_9);
    }

    #[test]
    fn multi_symbol_independent() {
        let mut cache = IndicatorCache::new();
        let candles_a = make_candles(200, 1000);
        let candles_b: Vec<Candle> = (0..200)
            .map(|i| {
                let t = 2000 + i as i64 * 60;
                let close = 250.0 + (i as f64) * 0.3 + ((i as f64) * 1.2).sin() * 5.0;
                make_candle(t, close)
            })
            .collect();
        let p = IndicatorPeriods::default();

        let a = cache.get_or_compute("AAPL", "5m", &candles_a, &p);
        let b = cache.get_or_compute("TSLA", "5m", &candles_b, &p);
        assert_eq!(cache.len(), 2);
        assert_ne!(a.ema_9, b.ema_9);
    }

    #[test]
    fn same_symbol_different_tf() {
        let mut cache = IndicatorCache::new();
        let candles = make_candles(200, 1000);
        let p = IndicatorPeriods::default();

        cache.get_or_compute("AAPL", "1m", &candles, &p);
        cache.get_or_compute("AAPL", "5m", &candles, &p);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn evict_stale_removes_old() {
        let mut cache = IndicatorCache::new();
        let candles = make_candles(200, 1000);
        let p = IndicatorPeriods::default();

        cache.get_or_compute("AAPL", "5m", &candles, &p);
        cache.evict_stale(std::time::Duration::from_secs(0));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn evict_stale_keeps_recent() {
        let mut cache = IndicatorCache::new();
        let candles = make_candles(200, 1000);
        let p = IndicatorPeriods::default();

        cache.get_or_compute("AAPL", "5m", &candles, &p);
        cache.evict_stale(std::time::Duration::from_secs(3600));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn empty_candles_returns_empty() {
        let mut cache = IndicatorCache::new();
        let result = cache.get_or_compute(
            "AAPL",
            "5m",
            &[],
            &IndicatorPeriods::default(),
        );
        assert!(result.ema_9.is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn indicator_cache_evict_stale_full_cleanup() {
        let mut cache = IndicatorCache::new();
        let candles = make_candles(200, 1000);
        let p = IndicatorPeriods::default();

        cache.get_or_compute("AAPL", "5m", &candles, &p);
        cache.get_or_compute("TSLA", "1m", &candles, &p);
        cache.get_or_compute("GOOG", "15m", &candles, &p);
        assert_eq!(cache.len(), 3);

        cache.evict_stale(Duration::from_secs(0));
        assert_eq!(cache.len(), 0, "zero-duration eviction must clear all entries");
    }

    /// S15.6 — verifies the chunked-tick refactor of
    /// `run_indicator_cache_evictor`: watchdog ticks at the 30s chunk
    /// cadence within a 300s window (eviction-cycle period).
    ///
    /// Tolerance is `9..=11` ticks because of the standard paused-time
    /// first-poll race: the spawned task only arms its first sleep on
    /// its first poll, which happens after the first `advance(30)` in
    /// some runtime schedulings. We care about cadence regression, not
    /// exact tick alignment.
    #[tokio::test(start_paused = true)]
    async fn evictor_ticks_at_30s_cadence_within_300s_window() {
        use crate::config::RuntimeConfig;

        let state = Arc::new(crate::state::TitanState::new(RuntimeConfig::default()));
        let market_loop = Arc::new(MarketDataState::new());

        let st_loop = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            run_indicator_cache_evictor(st_loop, market_loop).await;
        });

        // Yield once so the spawned task arms its first sleep before
        // we start advancing time.
        tokio::task::yield_now().await;

        for _ in 0..10 {
            tokio::time::advance(Duration::from_secs(30)).await;
            tokio::task::yield_now().await;
        }

        state.shutdown.cancel();
        tokio::time::advance(Duration::from_secs(30)).await;
        let _ = handle.await;

        let sys = state.system.lock().unwrap();
        let h = sys
            .loops
            .get("indicator_evictor")
            .expect("indicator_evictor loop registered");
        assert!(
            (9..=11).contains(&h.tick_count),
            "expected 9..=11 ticks for ~10 × 30s chunks; got {}",
            h.tick_count
        );
    }
}
