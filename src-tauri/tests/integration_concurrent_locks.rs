/// S05: Stress test for the two-tier lock hierarchy.
///
/// Mirrors the TitanState (Tier A: 7 mutexes) and MarketDataState
/// (Tier B: 4 mutexes) locking discipline. 10 threads hammer the locks
/// for 30 seconds to verify no deadlock occurs under contention.
///
/// Acceptance: all threads complete ≥100k iterations, no poison, no hang.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const TEST_DURATION_SECS: u64 = 30;
const MIN_ITERATIONS: u64 = 100_000;

struct TierA {
    config: Mutex<u64>,       // 1
    market: Mutex<String>,    // 2
    smarttrend: Mutex<f64>,   // 3
    trading: Mutex<Vec<u64>>, // 4
    risk: Mutex<u8>,          // 5
    journal: Mutex<Vec<u8>>,  // 6
    system: Mutex<u64>,       // 7
}

struct TierB {
    last_prices: Mutex<f64>,       // B1
    cvd: Mutex<f64>,               // B2
    candle_buffers: Mutex<Vec<u8>>,// B3
    indicator_cache: Mutex<u64>,   // B4
}

#[test]
fn concurrent_lock_stress_30s() {
    let stop = Arc::new(AtomicBool::new(false));
    let counters: Arc<Vec<AtomicU64>> = Arc::new(
        (0..10).map(|_| AtomicU64::new(0)).collect(),
    );

    let tier_a = Arc::new(TierA {
        config: Mutex::new(0),
        market: Mutex::new("AAPL".into()),
        smarttrend: Mutex::new(1000.0),
        trading: Mutex::new(vec![0]),
        risk: Mutex::new(0),
        journal: Mutex::new(Vec::new()),
        system: Mutex::new(0),
    });

    let tier_b = Arc::new(TierB {
        last_prices: Mutex::new(180.0),
        cvd: Mutex::new(0.0),
        candle_buffers: Mutex::new(Vec::new()),
        indicator_cache: Mutex::new(0),
    });

    let mut handles = Vec::new();

    // Thread A: config(1) only
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut g = a.config.lock().unwrap();
                *g = g.wrapping_add(1);
                drop(g);
                c[0].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread B: market(2) — mutate active_symbol
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            let syms = ["AAPL", "TSLA", "NVDA", "GOOG"];
            let mut i = 0usize;
            while !s.load(Ordering::Relaxed) {
                let mut g = a.market.lock().unwrap();
                *g = syms[i % syms.len()].into();
                i += 1;
                drop(g);
                c[1].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread C: smarttrend(3) — wallet topup/withdraw
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut g = a.smarttrend.lock().unwrap();
                *g += 1.0;
                *g -= 0.5;
                drop(g);
                c[2].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread D: trading(4) — push/retain Position with version bump
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut g = a.trading.lock().unwrap();
                let len = g.len() as u64;
                g.push(len);
                if g.len() > 100 {
                    g.retain(|v| v % 2 == 0);
                }
                drop(g);
                c[3].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread E: risk(5) — mutate breaker level
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut g = a.risk.lock().unwrap();
                *g = (*g).wrapping_add(1);
                drop(g);
                c[4].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread F: journal(6) — push entry
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut g = a.journal.lock().unwrap();
                g.push(0xAB);
                if g.len() > 200 {
                    g.clear();
                }
                drop(g);
                c[5].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread G: system(7) — tick
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let mut g = a.system.lock().unwrap();
                *g = g.wrapping_add(1);
                drop(g);
                c[6].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread H: full Tier-A cascade 1→2→3→4→5→6→7
    {
        let a = tier_a.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let g1 = a.config.lock().unwrap();
                let g2 = a.market.lock().unwrap();
                let g3 = a.smarttrend.lock().unwrap();
                let g4 = a.trading.lock().unwrap();
                let g5 = a.risk.lock().unwrap();
                let g6 = a.journal.lock().unwrap();
                let g7 = a.system.lock().unwrap();
                // All held — release in reverse
                drop(g7);
                drop(g6);
                drop(g5);
                drop(g4);
                drop(g3);
                drop(g2);
                drop(g1);
                c[7].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread I: Tier-B cascade B1→B2→B3→B4
    {
        let b = tier_b.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let g1 = b.last_prices.lock().unwrap();
                let g2 = b.cvd.lock().unwrap();
                let g3 = b.candle_buffers.lock().unwrap();
                let g4 = b.indicator_cache.lock().unwrap();
                drop(g4);
                drop(g3);
                drop(g2);
                drop(g1);
                c[8].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Thread J: cross-tier — Tier-A locks (drop), then Tier-B locks (drop)
    {
        let a = tier_a.clone();
        let b = tier_b.clone();
        let s = stop.clone();
        let c = counters.clone();
        handles.push(thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                // Tier A — ascending, then drop all
                {
                    let g1 = a.config.lock().unwrap();
                    let g2 = a.market.lock().unwrap();
                    drop(g2);
                    drop(g1);
                }
                // Boundary: all Tier-A released before Tier-B
                {
                    let g1 = b.last_prices.lock().unwrap();
                    let g2 = b.cvd.lock().unwrap();
                    drop(g2);
                    drop(g1);
                }
                c[9].fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Watchdog: run for TEST_DURATION_SECS then signal stop
    let start = Instant::now();
    let watchdog_counters = counters.clone();
    let watchdog_stop = stop.clone();

    let watchdog = thread::spawn(move || {
        let mut last_snapshot: Vec<u64> = vec![0; 10];
        loop {
            thread::sleep(Duration::from_secs(1));
            if start.elapsed() >= Duration::from_secs(TEST_DURATION_SECS) {
                watchdog_stop.store(true, Ordering::Relaxed);
                return;
            }
            // Check liveness: each thread must have progressed
            let current: Vec<u64> = watchdog_counters
                .iter()
                .map(|c| c.load(Ordering::Relaxed))
                .collect();
            for (i, (cur, prev)) in current.iter().zip(last_snapshot.iter()).enumerate() {
                if *cur == *prev && *cur > 0 {
                    // Allow one stall cycle — could be scheduler jitter
                    continue;
                }
                if start.elapsed() > Duration::from_secs(5) && *cur == 0 {
                    watchdog_stop.store(true, Ordering::Relaxed);
                    panic!("Thread {i} has 0 iterations after 5s — possible deadlock");
                }
            }
            last_snapshot = current;
        }
    });

    watchdog.join().expect("watchdog panicked");

    for (i, h) in handles.into_iter().enumerate() {
        h.join().unwrap_or_else(|_| panic!("Thread {i} panicked"));
    }

    // Verify each thread hit the minimum
    for (i, c) in counters.iter().enumerate() {
        let count = c.load(Ordering::Relaxed);
        assert!(
            count >= MIN_ITERATIONS,
            "Thread {i} only completed {count} iterations (minimum: {MIN_ITERATIONS})"
        );
    }

    println!("All 10 threads completed cleanly. Iteration counts:");
    for (i, c) in counters.iter().enumerate() {
        println!("  Thread {i}: {}", c.load(Ordering::Relaxed));
    }
}
