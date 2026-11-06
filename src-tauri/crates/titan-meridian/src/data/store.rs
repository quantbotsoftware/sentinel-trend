//! Local archive reader: Alpaca bar JSON lines (`.jsonl` or `.jsonl.gz`)
//! → RTH `DaySeries` per ET session. Extended-hours bars are dropped here;
//! they stay in the archive for the premarket research arm.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;

use crate::bars::{Bar, DaySeries};
use crate::session::{rth_index, scheduled_session_bars, FULL_SESSION_BARS};

#[derive(Debug, Deserialize)]
struct RawBar {
    t: String,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct LoadStats {
    pub lines: usize,
    pub rth_bars: usize,
    pub sessions_seen: usize,
    pub sessions_kept: usize,
    pub sessions_dropped_incomplete: usize,
}

fn open_reader(path: &Path) -> std::io::Result<Box<dyn Read>> {
    let f = File::open(path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Ok(Box::new(flate2::read::GzDecoder::new(f)))
    } else {
        Ok(Box::new(f))
    }
}

/// Load RTH sessions: 390 bars, or 210 on a scheduled early close
/// (`session::is_early_close` — prints after 13:00 ET on those days are
/// after-hours tape and are dropped). A session is kept when its last
/// scheduled bar is a real print and it has at least `min_real_bars`
/// real prints (scaled by 210/390 on early closes).
pub fn load_jsonl_days(path: &Path, min_real_bars: usize) -> std::io::Result<(Vec<DaySeries>, LoadStats)> {
    load_jsonl_days_live(path, min_real_bars, None)
}

/// Same as [`load_jsonl_days`], but the session dated `partial` (today, in
/// live replay) is kept even when incomplete: its length is the last bar
/// seen + 1 (capped at the scheduled length), forward-filled inside. Every
/// other day must be complete.
pub fn load_jsonl_days_live(path: &Path, min_real_bars: usize, partial: Option<NaiveDate>) -> std::io::Result<(Vec<DaySeries>, LoadStats)> {
    let reader = BufReader::new(open_reader(path)?);
    let mut by_day: BTreeMap<NaiveDate, Vec<(usize, Bar)>> = BTreeMap::new();
    let mut st = LoadStats::default();
    for line in reader.lines() {
        let line = line?;
        st.lines += 1;
        let Ok(rb) = serde_json::from_str::<RawBar>(&line) else { continue };
        let Ok(t) = DateTime::parse_from_rfc3339(&rb.t) else { continue };
        let Some((date, idx)) = rth_index(t.with_timezone(&Utc)) else { continue };
        st.rth_bars += 1;
        by_day.entry(date).or_default().push((idx, Bar { o: rb.o, h: rb.h, l: rb.l, c: rb.c, v: rb.v }));
    }
    st.sessions_seen = by_day.len();
    let mut days = Vec::with_capacity(by_day.len());
    for (date, sparse) in by_day {
        let len = scheduled_session_bars(date);
        if partial == Some(date) {
            let n = sparse.iter().map(|(i, _)| *i + 1).max().unwrap_or(0).min(len);
            if let Some(d) = DaySeries::from_sparse(date, n, &sparse) {
                days.push(d);
            }
            continue;
        }
        let has_last = sparse.iter().any(|(i, _)| *i == len - 1);
        let min_real = min_real_bars * len / FULL_SESSION_BARS;
        match DaySeries::from_sparse(date, len, &sparse) {
            Some(d) if has_last && d.real_bars >= min_real => days.push(d),
            _ => st.sessions_dropped_incomplete += 1,
        }
    }
    st.sessions_kept = days.len();
    Ok((days, st))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(lines: &[String]) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("meridian_store_{}_{seq}.jsonl", std::process::id()));
        let mut f = File::create(&p).unwrap_or_else(|e| panic!("{e}"));
        for l in lines {
            writeln!(f, "{l}").unwrap_or_else(|e| panic!("{e}"));
        }
        p
    }

    fn bar(ts: &str, c: f64) -> String {
        format!(r#"{{"t":"{ts}","o":{c},"h":{c},"l":{c},"c":{c},"v":100,"n":1,"vw":{c}}}"#)
    }

    #[test]
    fn loads_full_session_drops_extended_and_incomplete() {
        let mut lines = vec![bar("2026-04-29T12:00:00Z", 99.0)]; // 08:00 ET premarket → dropped
        for m in 0..390 {
            let h = 13 + (30 + m) / 60;
            let mi = (30 + m) % 60;
            lines.push(bar(&format!("2026-04-29T{h:02}:{mi:02}:00Z"), 100.0 + m as f64 * 0.01));
        }
        // next day: only 10 bars → dropped as incomplete
        for m in 0..10 {
            lines.push(bar(&format!("2026-04-30T13:{:02}:00Z", 30 + m), 101.0));
        }
        lines.push("not json".to_string());
        let p = write_tmp(&lines);
        let (days, st) = load_jsonl_days(&p, 300).unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&p);
        assert_eq!(st.lines, 402);
        assert_eq!(st.rth_bars, 400);
        assert_eq!(st.sessions_seen, 2);
        assert_eq!(st.sessions_kept, 1);
        assert_eq!(st.sessions_dropped_incomplete, 1);
        assert_eq!(days[0].date, NaiveDate::from_ymd_opt(2026, 4, 29).unwrap_or_default());
        assert_eq!(days[0].real_bars, 390);
        assert!((days[0].close() - 103.89).abs() < 1e-9);
    }

    #[test]
    fn early_close_session_is_210_bars_without_after_hours_tape() {
        // 2025-11-28 (day after Thanksgiving, EST): RTH 09:30–13:00 = 14:30–18:00Z,
        // then after-hours prints until 16:00 ET that the SIP feed also delivers.
        let mut lines = Vec::new();
        for m in 0..390 {
            let h = 14 + (30 + m) / 60;
            let mi = (30 + m) % 60;
            let px = if m < 210 { 600.0 + m as f64 * 0.01 } else { 590.0 };
            lines.push(bar(&format!("2025-11-28T{h:02}:{mi:02}:00Z"), px));
        }
        let p = write_tmp(&lines);
        let (days, st) = load_jsonl_days(&p, 300).unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&p);
        assert_eq!(st.sessions_kept, 1);
        assert_eq!(days[0].len(), 210, "early close: 210 scheduled bars");
        assert_eq!(days[0].real_bars, 210);
        assert!((days[0].close() - 602.09).abs() < 1e-9, "close is the 12:59 bar, not the after-hours print");
        // the same prints on a regular date keep all 390 bars
        let lines2: Vec<String> = lines.iter().map(|l| l.replace("2025-11-28", "2025-11-26")).collect();
        let p2 = write_tmp(&lines2);
        let (days2, _) = load_jsonl_days(&p2, 300).unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&p2);
        assert_eq!(days2[0].len(), 390);
        // an early close whose 12:59 bar is missing is dropped like any incomplete day
        let lines3: Vec<String> = lines.iter().filter(|l| !l.contains("T17:59:00Z")).cloned().collect();
        let p3 = write_tmp(&lines3);
        let (days3, st3) = load_jsonl_days(&p3, 300).unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&p3);
        assert!(days3.is_empty() && st3.sessions_dropped_incomplete == 1);
    }

    #[test]
    fn live_loader_keeps_todays_partial_session() {
        let mut lines = Vec::new();
        for m in 0..390 {
            let h = 13 + (30 + m) / 60;
            let mi = (30 + m) % 60;
            lines.push(bar(&format!("2026-04-29T{h:02}:{mi:02}:00Z"), 100.0));
        }
        // today: bars at 09:30, 09:31 and 09:35 (gap 09:32–09:34) → 6 bars, 3 real
        for m in [0, 1, 5] {
            lines.push(bar(&format!("2026-04-30T13:{:02}:00Z", 30 + m), 101.0 + m as f64));
        }
        let p = write_tmp(&lines);
        let today = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap_or_default();
        let (days, st) = load_jsonl_days_live(&p, 300, Some(today)).unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_file(&p);
        assert_eq!(st.sessions_dropped_incomplete, 0);
        assert_eq!(days.len(), 2);
        let t = &days[1];
        assert_eq!((t.date, t.len(), t.real_bars), (today, 6, 3));
        assert_eq!(t.bars[3].c, 102.0, "forward-filled from the 09:31 bar");
        assert_eq!(t.close(), 106.0);
    }
}
