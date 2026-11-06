//! Tiny deterministic PRNG (xorshift64*) so simulations are reproducible
//! without pulling `rand` into a pure crate.

#[derive(Debug, Clone)]
pub struct XorShift {
    s: u64,
}

impl XorShift {
    pub fn new(seed: u64) -> Self {
        Self { s: seed.max(1) ^ 0x9E37_79B9_7F4A_7C15 }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.s;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.s = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform integer in [0, n).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_f64() * n as f64) as usize
        }
    }

    /// Standard normal via Box–Muller.
    pub fn normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-300);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_uniform() {
        let mut a = XorShift::new(42);
        let mut b = XorShift::new(42);
        assert_eq!(a.next_u64(), b.next_u64());
        let mut r = XorShift::new(7);
        let n = 20_000;
        let mean = (0..n).map(|_| r.next_f64()).sum::<f64>() / n as f64;
        assert!((mean - 0.5).abs() < 0.01);
        let z = (0..n).map(|_| r.normal()).collect::<Vec<_>>();
        let m = z.iter().sum::<f64>() / n as f64;
        let v = z.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n as f64;
        assert!(m.abs() < 0.03 && (v - 1.0).abs() < 0.05);
        assert!(r.below(10) < 10);
    }
}
