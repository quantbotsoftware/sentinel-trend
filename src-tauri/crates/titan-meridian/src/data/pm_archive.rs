//! Premarket research archive: Benzinga headlines (`news_pm_*.jsonl`) and
//! per-day multi-symbol extended-hours bars (`min_YYYY-MM-DD.jsonl`) plus
//! trailing daily bars (`day_YYYY-MM-DD.jsonl`) as pulled by the reference
//! downloader (`meridian/raw/dl_pm_bars_reference.py`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;

use crate::catalyst::{classify, CatalystClass};
use crate::session::{et_date, ext_index};
use crate::sleeves::ExtBar;

#[derive(Debug, Clone, PartialEq)]
pub struct NewsEvent {
    pub date: NaiveDate,
    /// Extended-session minute index (0 = 04:00 ET).
    pub idx: usize,
    pub symbol: String,
    pub class: CatalystClass,
    pub headline: String,
}

#[derive(Debug, Deserialize)]
struct RawNews {
    t: String,
    s: Vec<String>,
    h: String,
}

fn open(path: &Path) -> std::io::Result<Box<dyn Read>> {
    let f = File::open(path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        Ok(Box::new(flate2::read::GzDecoder::new(f)))
    } else {
        Ok(Box::new(f))
    }
}

/// US common-stock ticker heuristic: 1–5 uppercase letters.
pub fn is_us_symbol(s: &str) -> bool {
    !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// Load and classify headlines. Headlines before 04:00 ET on the same
/// day are mapped to index 0 (the tape is watched from 04:00).
pub fn load_news(path: &Path) -> std::io::Result<Vec<NewsEvent>> {
    let rd = BufReader::new(open(path)?);
    let mut out = Vec::new();
    for line in rd.lines() {
        let line = line?;
        let Ok(r) = serde_json::from_str::<RawNews>(&line) else { continue };
        let Ok(t) = DateTime::parse_from_rfc3339(&r.t) else { continue };
        let t = t.with_timezone(&Utc);
        let (date, idx) = match ext_index(t) {
            Some(x) => x,
            None => (et_date(t), 0),
        };
        let class = classify(&r.h);
        for s in r.s.iter().filter(|s| is_us_symbol(s)) {
            out.push(NewsEvent { date, idx, symbol: s.clone(), class, headline: r.h.clone() });
        }
    }
    out.sort_by(|a, b| (a.date, a.idx, a.symbol.as_str()).cmp(&(b.date, b.idx, b.symbol.as_str())));
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RawBar {
    t: String,
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
    #[serde(default)]
    n: f64,
    #[serde(rename = "S")]
    s: String,
}

#[derive(Debug, Default)]
pub struct DayArchive {
    /// Extended bars 04:00–10:30 per symbol, sorted by idx.
    pub bars: HashMap<String, Vec<ExtBar>>,
    /// Previous session close per symbol (last daily bar strictly before the date).
    pub prev_close: HashMap<String, f64>,
    /// Mean daily true range over the trailing daily bars (≈ ATR).
    pub atr: HashMap<String, f64>,
}

pub fn load_day(dir: &Path, date: NaiveDate) -> std::io::Result<Option<DayArchive>> {
    let fmin = dir.join(format!("min_{date}.jsonl"));
    let fday = dir.join(format!("day_{date}.jsonl"));
    if !fmin.exists() || !fday.exists() {
        return Ok(None);
    }
    let mut arch = DayArchive::default();
    for line in BufReader::new(open(&fmin)?).lines() {
        let line = line?;
        let Ok(b) = serde_json::from_str::<RawBar>(&line) else { continue };
        let Ok(t) = DateTime::parse_from_rfc3339(&b.t) else { continue };
        let Some((d, idx)) = ext_index(t.with_timezone(&Utc)) else { continue };
        if d != date {
            continue;
        }
        arch.bars.entry(b.s).or_default().push(ExtBar { idx, o: b.o, h: b.h, l: b.l, c: b.c, v: b.v, n: b.n });
    }
    for v in arch.bars.values_mut() {
        v.sort_by_key(|b| b.idx);
        v.dedup_by_key(|b| b.idx);
    }
    // daily bars: group per symbol in time order
    let mut daily: HashMap<String, Vec<(NaiveDate, RawBar)>> = HashMap::new();
    for line in BufReader::new(open(&fday)?).lines() {
        let line = line?;
        let Ok(b) = serde_json::from_str::<RawBar>(&line) else { continue };
        let Ok(t) = DateTime::parse_from_rfc3339(&b.t) else { continue };
        let d = et_date(t.with_timezone(&Utc));
        if d < date {
            daily.entry(b.s.clone()).or_default().push((d, b));
        }
    }
    for (sym, mut v) in daily {
        v.sort_by_key(|x| x.0);
        if let Some((_, last)) = v.last() {
            arch.prev_close.insert(sym.clone(), last.c);
        }
        let trs: Vec<f64> = v
            .windows(2)
            .map(|w| {
                let (p, c) = (&w[0].1, &w[1].1);
                (c.h - c.l).max((c.h - p.c).abs()).max((c.l - p.c).abs())
            })
            .collect();
        if !trs.is_empty() {
            arch.atr.insert(sym, trs.iter().sum::<f64>() / trs.len() as f64);
        }
    }
    Ok(Some(arch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str, lines: &[String]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("meridian_pm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        p
    }

    #[test]
    fn news_loader_classifies_and_maps_time() {
        let p = tmp(
            "news.jsonl",
            &[
                r#"{"t":"2026-09-03T09:45:12Z","s":["DLTH","TSX:ABC"],"h":"Duluth Holdings Q2 EPS $0.50 Beats $(0.01) Estimate, Sales $150M Beat $140M Estimate"}"#.into(),
                r#"{"t":"2026-09-03T07:40:00Z","s":["EARLY"],"h":"Early Co Wins $10M Contract"}"#.into(),
                "garbage".into(),
            ],
        );
        let ev = load_news(&p).unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].symbol, "EARLY");
        assert_eq!(ev[0].idx, 0, "03:40 ET maps to the 04:00 index");
        assert_eq!(ev[1].symbol, "DLTH");
        assert_eq!(ev[1].idx, 105, "05:45 EDT");
        assert_eq!(ev[1].class, CatalystClass::EarningsBeat);
        assert!(!is_us_symbol("TSX:ABC") && is_us_symbol("A") && !is_us_symbol("TOOLONG"));
    }

    #[test]
    fn day_loader_groups_bars_and_prev_close() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 3).unwrap();
        let dir = tmp(
            "min_2026-09-03.jsonl",
            &[
                r#"{"c":3.1,"h":3.2,"l":3.0,"n":5,"o":3.0,"t":"2026-09-03T09:49:00Z","v":100,"S":"DLTH"}"#.into(),
                r#"{"c":3.0,"h":3.0,"l":3.0,"n":1,"o":3.0,"t":"2026-09-03T08:00:00Z","v":10,"S":"DLTH"}"#.into(),
                r#"{"c":9.0,"h":9.0,"l":9.0,"n":1,"o":9.0,"t":"2026-09-03T08:00:00Z","v":10,"S":"OTHER"}"#.into(),
            ],
        )
        .parent()
        .unwrap()
        .to_path_buf();
        tmp(
            "day_2026-09-03.jsonl",
            &[
                r#"{"c":2.8,"h":2.9,"l":2.7,"n":1,"o":2.8,"t":"2026-09-01T04:00:00Z","v":10,"S":"DLTH"}"#.into(),
                r#"{"c":3.0,"h":3.1,"l":2.75,"n":1,"o":2.8,"t":"2026-09-02T04:00:00Z","v":10,"S":"DLTH"}"#.into(),
                r#"{"c":99.0,"h":99.0,"l":99.0,"n":1,"o":99.0,"t":"2026-09-03T04:00:00Z","v":10,"S":"DLTH"}"#.into(),
            ],
        );
        let a = load_day(&dir, date).unwrap().unwrap();
        let d = &a.bars["DLTH"];
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].idx, d[1].idx), (0, 109), "sorted by index");
        assert_eq!(a.prev_close["DLTH"], 3.0, "same-day daily bar is ignored");
        assert!((a.atr["DLTH"] - 0.35).abs() < 1e-9, "TR = max(3.1−2.75, |3.1−2.8|, |2.75−2.8|)");
        assert!(load_day(&dir, NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()).unwrap().is_none());
    }
}
