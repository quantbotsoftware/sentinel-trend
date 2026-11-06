//! Daily close series and the aligned panel the simulator runs on.
//!
//! Only the close is used (signal, fill, marking) — pre-registration §Date.
//! Two loaders: Alpaca daily JSONL (`{"c":…,"t":"2016-01-04T05:00:00Z"}`)
//! and Stooq CSV (`Date,Open,High,Low,Close,Volume`), the latter only for
//! the labelled robustness pass that does not enter the gate.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use chrono::{Datelike, NaiveDate, Weekday};
use serde::Deserialize;

/// One instrument's daily closes, ascending by date.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub symbol: String,
    pub rows: Vec<(NaiveDate, f64)>,
}

#[derive(Deserialize)]
struct AlpacaBar {
    c: f64,
    t: String,
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// Alpaca `/v2/stocks/bars` daily rows, one JSON object per line.
pub fn load_alpaca_jsonl(path: &Path, symbol: &str) -> io::Result<Series> {
    let reader = BufReader::new(File::open(path)?);
    let mut rows = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let Ok(b) = serde_json::from_str::<AlpacaBar>(&line) else { continue };
        if let Some(d) = parse_date(&b.t) {
            if b.c.is_finite() && b.c > 0.0 {
                rows.push((d, b.c));
            }
        }
    }
    rows.sort_by_key(|r| r.0);
    rows.dedup_by_key(|r| r.0);
    Ok(Series { symbol: symbol.to_uppercase(), rows })
}

/// Stooq `Date,Open,High,Low,Close,Volume` export (header line skipped).
pub fn load_stooq_csv(path: &Path, symbol: &str) -> io::Result<Series> {
    let reader = BufReader::new(File::open(path)?);
    let mut rows = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 5 {
            continue;
        }
        let (Some(d), Ok(c)) = (parse_date(cols[0]), cols[4].trim().parse::<f64>()) else { continue };
        if c.is_finite() && c > 0.0 {
            rows.push((d, c));
        }
    }
    rows.sort_by_key(|r| r.0);
    rows.dedup_by_key(|r| r.0);
    Ok(Series { symbol: symbol.to_uppercase(), rows })
}

/// Sessions × instruments close matrix on a master calendar. An instrument
/// missing a calendar date carries its last close forward once it has
/// started trading; before its first close it is `None` (flat, no signal).
#[derive(Debug, Clone, PartialEq)]
pub struct Panel {
    pub symbols: Vec<String>,
    pub dates: Vec<NaiveDate>,
    /// `closes[t][i]`
    pub closes: Vec<Vec<Option<f64>>>,
}

impl Panel {
    pub fn build(calendar: &[NaiveDate], series: &[Series]) -> Panel {
        let maps: Vec<HashMap<NaiveDate, f64>> = series.iter().map(|s| s.rows.iter().copied().collect()).collect();
        let mut last: Vec<Option<f64>> = vec![None; series.len()];
        let mut closes = Vec::with_capacity(calendar.len());
        for d in calendar {
            let mut row = Vec::with_capacity(series.len());
            for (i, m) in maps.iter().enumerate() {
                if let Some(c) = m.get(d) {
                    last[i] = Some(*c);
                }
                row.push(last[i]);
            }
            closes.push(row);
        }
        Panel { symbols: series.iter().map(|s| s.symbol.clone()).collect(), dates: calendar.to_vec(), closes }
    }

    pub fn n_days(&self) -> usize {
        self.dates.len()
    }

    pub fn n_symbols(&self) -> usize {
        self.symbols.len()
    }

    /// Close of instrument `i` on session `t`.
    pub fn close(&self, t: usize, i: usize) -> Option<f64> {
        self.closes.get(t).and_then(|r| r.get(i).copied().flatten())
    }

    /// First session index on which **every** instrument has `lookback + 1`
    /// closes available (the pre-registered common start for both variants).
    pub fn first_full_history(&self, lookback: usize) -> Option<usize> {
        let starts: Vec<usize> = (0..self.n_symbols())
            .map(|i| (0..self.n_days()).find(|&t| self.close(t, i).is_some()).unwrap_or(self.n_days()))
            .collect();
        let latest = *starts.iter().max()?;
        let t = latest + lookback;
        (t < self.n_days()).then_some(t)
    }

    /// `true` on the last session of each ISO week (Friday, or the day before
    /// a Friday holiday). The final date counts only if it is a Friday.
    pub fn week_ends(&self) -> Vec<bool> {
        let n = self.n_days();
        (0..n)
            .map(|t| match self.dates.get(t + 1) {
                Some(next) => next.iso_week() != self.dates[t].iso_week(),
                None => self.dates[t].weekday() == Weekday::Fri,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn tmp(name: &str, body: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("titan_frontier_{name}_{}", std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn alpaca_loader_reads_close_and_date_sorted_deduped() {
        let p = tmp(
            "alp",
            r#"{"c":170.97,"t":"2016-01-05T05:00:00Z","v":1}
{"c":170.68,"t":"2016-01-04T05:00:00Z","v":1}
garbage
{"c":170.68,"t":"2016-01-04T05:00:00Z","v":1}
"#,
        );
        let s = load_alpaca_jsonl(&p, "spy").unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(s.symbol, "SPY");
        assert_eq!(s.rows, vec![(d("2016-01-04"), 170.68), (d("2016-01-05"), 170.97)]);
    }

    #[test]
    fn stooq_loader_uses_close_column() {
        let p = tmp("stooq", "Date,Open,High,Low,Close,Volume\n2008-10-10,1,2,0.5,1.5,100\n2008-10-13,1,2,0.5,1.7,100\n");
        let s = load_stooq_csv(&p, "iwm").unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(s.rows, vec![(d("2008-10-10"), 1.5), (d("2008-10-13"), 1.7)]);
    }

    #[test]
    fn panel_forward_fills_after_start_and_is_none_before() {
        let cal = vec![d("2016-01-04"), d("2016-01-05"), d("2016-01-06"), d("2016-01-07")];
        let a = Series { symbol: "A".into(), rows: vec![(d("2016-01-04"), 1.0), (d("2016-01-05"), 2.0), (d("2016-01-07"), 4.0)] };
        let b = Series { symbol: "B".into(), rows: vec![(d("2016-01-06"), 10.0), (d("2016-01-07"), 11.0)] };
        let p = Panel::build(&cal, &[a, b]);
        assert_eq!(p.close(2, 0), Some(2.0), "A carried forward over the missing 01-06");
        assert_eq!(p.close(3, 0), Some(4.0));
        assert_eq!(p.close(0, 1), None, "B has not started");
        assert_eq!(p.close(2, 1), Some(10.0));
        // B starts at t=2; with lookback 1 the common start is t=3
        assert_eq!(p.first_full_history(1), Some(3));
        assert_eq!(p.first_full_history(2), None);
    }

    #[test]
    fn week_ends_mark_friday_or_pre_holiday_thursday() {
        // 2026-04-03 is Good Friday (closed): Thu 04-02 closes the ISO week
        let cal = vec![d("2026-03-30"), d("2026-03-31"), d("2026-04-01"), d("2026-04-02"), d("2026-04-06"), d("2026-04-07"), d("2026-04-08")];
        let p = Panel::build(&cal, &[]);
        assert_eq!(p.week_ends(), vec![false, false, false, true, false, false, false], "final Wednesday is not a rebalance day");
        let cal2 = vec![d("2026-09-14"), d("2026-09-18")];
        assert_eq!(Panel::build(&cal2, &[]).week_ends(), vec![false, true]);
    }
}
