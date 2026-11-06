//! Daily $ P/L curves: loading the A+B curve written by `meridian backtest
//! --csv`, aligning on common dates, and the summary statistics the gate
//! uses (Sharpe on non-compounded returns, drawdown on cumulative $).

use std::io::{self, BufRead, BufReader};
use std::path::Path;

use chrono::{Datelike, NaiveDate};
use titan_meridian::stats::performance;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Curve {
    pub dates: Vec<NaiveDate>,
    /// $ P/L per session
    pub pnl: Vec<f64>,
}

impl Curve {
    pub fn new(dates: Vec<NaiveDate>, pnl: Vec<f64>) -> Self {
        Self { dates, pnl }
    }

    /// `meridian backtest --csv`: `date,equity,ret`. With fixed sizing the $
    /// P/L is the equity difference (the `ret` column is on compounded
    /// equity and is not used — chat 5 L30). The first row has no P/L.
    pub fn load_meridian_csv(path: &Path) -> io::Result<Curve> {
        let reader = BufReader::new(std::fs::File::open(path)?);
        let mut dates = Vec::new();
        let mut pnl = Vec::new();
        let mut prev: Option<f64> = None;
        for line in reader.lines() {
            let line = line?;
            let cols: Vec<&str> = line.split(',').collect();
            if cols.len() < 2 {
                continue;
            }
            let (Ok(d), Ok(eq)) = (NaiveDate::parse_from_str(cols[0], "%Y-%m-%d"), cols[1].trim().parse::<f64>()) else { continue };
            if let Some(p) = prev {
                dates.push(d);
                pnl.push(eq - p);
            }
            prev = Some(eq);
        }
        Ok(Curve { dates, pnl })
    }

    /// Restrict to `[from, to]` inclusive.
    pub fn slice(&self, from: NaiveDate, to: NaiveDate) -> Curve {
        let mut c = Curve::default();
        for (d, p) in self.dates.iter().zip(&self.pnl) {
            if *d >= from && *d <= to {
                c.dates.push(*d);
                c.pnl.push(*p);
            }
        }
        c
    }

    /// Common dates of two curves, in order: `(dates, a, b)`.
    pub fn align(a: &Curve, b: &Curve) -> (Vec<NaiveDate>, Vec<f64>, Vec<f64>) {
        let mut out = (Vec::new(), Vec::new(), Vec::new());
        let (mut i, mut j) = (0, 0);
        while i < a.dates.len() && j < b.dates.len() {
            match a.dates[i].cmp(&b.dates[j]) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.0.push(a.dates[i]);
                    out.1.push(a.pnl[i]);
                    out.2.push(b.pnl[j]);
                    i += 1;
                    j += 1;
                }
            }
        }
        out
    }

    /// $ P/L per calendar year, in order.
    pub fn by_year(&self) -> Vec<(i32, f64)> {
        let mut out: Vec<(i32, f64)> = Vec::new();
        for (d, p) in self.dates.iter().zip(&self.pnl) {
            match out.last_mut() {
                Some((y, s)) if *y == d.year() => *s += p,
                _ => out.push((d.year(), *p)),
            }
        }
        out
    }

    /// $ P/L per calendar month `(year, month, pnl)`.
    pub fn by_month(&self) -> Vec<(i32, u32, f64)> {
        let mut out: Vec<(i32, u32, f64)> = Vec::new();
        for (d, p) in self.dates.iter().zip(&self.pnl) {
            match out.last_mut() {
                Some((y, m, s)) if *y == d.year() && *m == d.month() => *s += p,
                _ => out.push((d.year(), d.month(), *p)),
            }
        }
        out
    }
}

/// Annualised Sharpe of non-compounded returns `pnl / capital`.
pub fn sharpe(pnl: &[f64], capital: f64) -> f64 {
    let r: Vec<f64> = pnl.iter().map(|p| p / capital).collect();
    performance(&r).sharpe
}

/// Annualised volatility of `pnl / capital`.
pub fn ann_vol(pnl: &[f64], capital: f64) -> f64 {
    let r: Vec<f64> = pnl.iter().map(|p| p / capital).collect();
    performance(&r).ann_vol
}

/// Deepest peak-to-trough fall of the cumulative $ P/L (≤ 0).
pub fn max_drawdown_dollars(pnl: &[f64]) -> f64 {
    let (mut cum, mut peak, mut mdd) = (0.0f64, 0.0f64, 0.0f64);
    for p in pnl {
        cum += p;
        peak = peak.max(cum);
        mdd = mdd.min(cum - peak);
    }
    mdd
}

/// Sample standard deviation (N − 1).
pub fn stdev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = xs.iter().sum::<f64>() / xs.len() as f64;
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() as f64 - 1.0)).sqrt()
}

/// Pearson correlation of two equally long series.
pub fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    if n < 2 {
        return 0.0;
    }
    let (ma, mb) = (a[..n].iter().sum::<f64>() / n as f64, b[..n].iter().sum::<f64>() / n as f64);
    let cov: f64 = (0..n).map(|i| (a[i] - ma) * (b[i] - mb)).sum();
    let (va, vb): (f64, f64) = ((0..n).map(|i| (a[i] - ma).powi(2)).sum(), (0..n).map(|i| (b[i] - mb).powi(2)).sum());
    if va <= 0.0 || vb <= 0.0 {
        0.0
    } else {
        cov / (va * vb).sqrt()
    }
}

/// OLS slope of `y` on `x` (beta).
pub fn beta(y: &[f64], x: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n < 2 {
        return 0.0;
    }
    let (mx, my) = (x[..n].iter().sum::<f64>() / n as f64, y[..n].iter().sum::<f64>() / n as f64);
    let cov: f64 = (0..n).map(|i| (x[i] - mx) * (y[i] - my)).sum();
    let vx: f64 = (0..n).map(|i| (x[i] - mx).powi(2)).sum();
    if vx <= 0.0 {
        0.0
    } else {
        cov / vx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn meridian_csv_gives_equity_differences() {
        let p = std::env::temp_dir().join(format!("titan_frontier_curve_{}", std::process::id()));
        std::fs::File::create(&p).unwrap().write_all(b"date,equity,ret\n2016-02-01,100000.00,0.0\n2016-02-02,100250.00,0.0025\n2016-02-03,100100.00,-0.001\n").unwrap();
        let c = Curve::load_meridian_csv(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        assert_eq!(c.dates, vec![d("2016-02-02"), d("2016-02-03")]);
        assert!((c.pnl[0] - 250.0).abs() < 1e-9 && (c.pnl[1] + 150.0).abs() < 1e-9);
    }

    #[test]
    fn align_keeps_only_common_dates_in_order() {
        let a = Curve::new(vec![d("2020-01-02"), d("2020-01-03"), d("2020-01-06")], vec![1.0, 2.0, 3.0]);
        let b = Curve::new(vec![d("2020-01-03"), d("2020-01-06"), d("2020-01-07")], vec![10.0, 20.0, 30.0]);
        let (dates, x, y) = Curve::align(&a, &b);
        assert_eq!(dates, vec![d("2020-01-03"), d("2020-01-06")]);
        assert_eq!(x, vec![2.0, 3.0]);
        assert_eq!(y, vec![10.0, 20.0]);
    }

    #[test]
    fn drawdown_and_correlation_and_beta() {
        assert!((max_drawdown_dollars(&[100.0, -50.0, -80.0, 200.0]) + 130.0).abs() < 1e-12);
        assert_eq!(max_drawdown_dollars(&[1.0, 1.0]), 0.0);
        let a = [1.0, 2.0, 3.0, 4.0];
        let b = [2.0, 4.0, 6.0, 8.0];
        assert!((correlation(&a, &b) - 1.0).abs() < 1e-12);
        assert!((beta(&b, &a) - 2.0).abs() < 1e-12);
        let c = [4.0, 3.0, 2.0, 1.0];
        assert!((correlation(&a, &c) + 1.0).abs() < 1e-12);
        assert_eq!(correlation(&a, &[1.0, 1.0, 1.0, 1.0]), 0.0);
    }

    #[test]
    fn yearly_and_monthly_buckets() {
        let c = Curve::new(vec![d("2019-12-31"), d("2020-01-02"), d("2020-01-31"), d("2020-02-03")], vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(c.by_year(), vec![(2019, 1.0), (2020, 9.0)]);
        assert_eq!(c.by_month(), vec![(2019, 12, 1.0), (2020, 1, 5.0), (2020, 2, 4.0)]);
        assert_eq!(c.slice(d("2020-01-01"), d("2020-01-31")).pnl, vec![2.0, 3.0]);
    }

    #[test]
    fn sharpe_is_scale_invariant_and_uses_capital() {
        let pnl: Vec<f64> = (0..300).map(|k| if k % 3 == 0 { -100.0 } else { 120.0 }).collect();
        let s1 = sharpe(&pnl, 100_000.0);
        let s2 = sharpe(&pnl.iter().map(|p| p * 3.0).collect::<Vec<_>>(), 100_000.0);
        assert!((s1 - s2).abs() < 1e-9 && s1 > 0.0);
        assert!((stdev(&[1.0, 2.0, 3.0, 4.0]) - (5.0f64 / 3.0).sqrt()).abs() < 1e-12);
    }
}
