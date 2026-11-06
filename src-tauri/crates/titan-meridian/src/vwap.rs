//! Incremental session VWAP on typical price (H+L+C)/3.

use crate::bars::Bar;

#[derive(Debug, Default, Clone, Copy)]
pub struct Vwap {
    cum_pv: f64,
    cum_v: f64,
    last: f64,
}

impl Vwap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one bar, return the VWAP after it. Zero-volume bars leave the
    /// VWAP unchanged (forward-filled gaps carry no information).
    pub fn push(&mut self, b: &Bar) -> f64 {
        if b.v > 0.0 {
            let tp = (b.h + b.l + b.c) / 3.0;
            self.cum_pv += tp * b.v;
            self.cum_v += b.v;
            self.last = self.cum_pv / self.cum_v;
        } else if self.cum_v == 0.0 {
            self.last = b.c;
        }
        self.last
    }

    pub fn value(&self) -> f64 {
        self.last
    }

    /// Full-session VWAP path (one value per bar).
    pub fn series(bars: &[Bar]) -> Vec<f64> {
        let mut v = Vwap::new();
        bars.iter().map(|b| v.push(b)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vwap_weights_by_volume_and_ignores_zero_volume() {
        let bars = [
            Bar { o: 10.0, h: 11.0, l: 9.0, c: 10.0, v: 100.0 },  // tp 10
            Bar { o: 10.0, h: 13.0, l: 11.0, c: 12.0, v: 300.0 }, // tp 12
            Bar { o: 12.0, h: 12.0, l: 12.0, c: 12.0, v: 0.0 },   // gap
        ];
        let s = Vwap::series(&bars);
        assert!((s[0] - 10.0).abs() < 1e-12);
        assert!((s[1] - 11.5).abs() < 1e-12);
        assert!((s[2] - 11.5).abs() < 1e-12);
    }

    #[test]
    fn leading_zero_volume_uses_close() {
        let mut v = Vwap::new();
        assert_eq!(v.push(&Bar { o: 5.0, h: 5.0, l: 5.0, c: 5.0, v: 0.0 }), 5.0);
    }
}
