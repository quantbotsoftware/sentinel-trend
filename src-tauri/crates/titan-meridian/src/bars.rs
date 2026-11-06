//! Bar data model: 1-minute bars and forward-filled RTH day series.

use chrono::NaiveDate;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
}

/// One RTH session of a symbol, dense (one bar per minute, gaps
/// forward-filled with zero volume). `bars.len()` is 390 or 210.
#[derive(Debug, Clone, PartialEq)]
pub struct DaySeries {
    pub date: NaiveDate,
    pub bars: Vec<Bar>,
    /// Number of minutes that had a real print (before forward-fill).
    pub real_bars: usize,
}

impl DaySeries {
    /// Build from sparse (index, bar) pairs. Returns `None` when the first
    /// bar is missing (no open) — a day without an opening print is unusable.
    pub fn from_sparse(date: NaiveDate, session_len: usize, sparse: &[(usize, Bar)]) -> Option<Self> {
        let mut slots: Vec<Option<Bar>> = vec![None; session_len];
        let mut real = 0usize;
        for (i, b) in sparse {
            if *i < session_len {
                if slots[*i].is_none() {
                    real += 1;
                }
                slots[*i] = Some(*b);
            }
        }
        let first = slots.first().copied().flatten()?;
        let mut bars = Vec::with_capacity(session_len);
        let mut last = first;
        for s in slots {
            match s {
                Some(b) => {
                    bars.push(b);
                    last = b;
                }
                None => bars.push(Bar { o: last.c, h: last.c, l: last.c, c: last.c, v: 0.0 }),
            }
        }
        Some(Self { date, bars, real_bars: real })
    }

    pub fn open(&self) -> f64 {
        self.bars.first().map(|b| b.o).unwrap_or(f64::NAN)
    }
    pub fn close(&self) -> f64 {
        self.bars.last().map(|b| b.c).unwrap_or(f64::NAN)
    }
    pub fn high(&self) -> f64 {
        self.bars.iter().map(|b| b.h).fold(f64::MIN, f64::max)
    }
    pub fn low(&self) -> f64 {
        self.bars.iter().map(|b| b.l).fold(f64::MAX, f64::min)
    }
    pub fn volume(&self) -> f64 {
        self.bars.iter().map(|b| b.v).sum()
    }
    pub fn len(&self) -> usize {
        self.bars.len()
    }
    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }
    /// Internal Bar Strength of the session: (C − L)/(H − L); 0.5 when flat.
    pub fn ibs(&self) -> f64 {
        let (h, l, c) = (self.high(), self.low(), self.close());
        if h > l {
            (c - l) / (h - l)
        } else {
            0.5
        }
    }
    /// True range vs previous close.
    pub fn true_range(&self, prev_close: f64) -> f64 {
        let (h, l) = (self.high(), self.low());
        (h - l).max((h - prev_close).abs()).max((l - prev_close).abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(c: f64) -> Bar {
        Bar { o: c, h: c + 1.0, l: c - 1.0, c, v: 10.0 }
    }

    #[test]
    fn forward_fill_and_counts() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        let s = DaySeries::from_sparse(d, 5, &[(0, b(10.0)), (2, b(12.0)), (4, b(11.0))]).unwrap();
        assert_eq!(s.len(), 5);
        assert_eq!(s.real_bars, 3);
        assert_eq!(s.bars[1], Bar { o: 10.0, h: 10.0, l: 10.0, c: 10.0, v: 0.0 });
        assert_eq!(s.bars[3].c, 12.0);
        assert_eq!(s.volume(), 30.0);
        assert_eq!(s.open(), 10.0);
        assert_eq!(s.close(), 11.0);
        assert_eq!(s.high(), 13.0);
        assert_eq!(s.low(), 9.0);
        // IBS = (11 − 9)/(13 − 9) = 0.5
        assert!((s.ibs() - 0.5).abs() < 1e-12);
        assert!((s.true_range(20.0) - 11.0).abs() < 1e-12);
    }

    #[test]
    fn missing_open_is_unusable() {
        let d = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        assert!(DaySeries::from_sparse(d, 5, &[(1, b(10.0))]).is_none());
    }
}
