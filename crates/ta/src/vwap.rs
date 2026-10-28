/// A weighted mean and variance, updated with West's numerically stable
/// algorithm so a full session of ticks does not lose precision to
/// cancellation.
#[derive(Debug, Clone, Default)]
pub struct WeightedStats {
    weight: f64,
    mean: f64,
    m2: f64,
}

impl WeightedStats {
    pub fn update(&mut self, x: f64, w: f64) {
        if w <= 0.0 || !w.is_finite() || !x.is_finite() {
            return;
        }
        self.weight += w;
        let delta = x - self.mean;
        self.mean += delta * w / self.weight;
        self.m2 += w * delta * (x - self.mean);
    }

    pub fn weight(&self) -> f64 {
        self.weight
    }

    pub fn mean(&self) -> Option<f64> {
        (self.weight > 0.0).then_some(self.mean)
    }

    /// Population (frequency-weighted) standard deviation.
    pub fn std_dev(&self) -> Option<f64> {
        (self.weight > 0.0).then(|| (self.m2 / self.weight).max(0.0).sqrt())
    }
}

/// Session VWAP with volume-weighted standard-deviation bands.
///
/// Feed it each trade (or each bar's typical price) with its volume, and call
/// [`VwapBands::reset`] at the start of every session.
#[derive(Debug, Clone, Default)]
pub struct VwapBands {
    stats: WeightedStats,
}

impl VwapBands {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, price: f64, volume: f64) {
        self.stats.update(price, volume);
    }

    pub fn vwap(&self) -> Option<f64> {
        self.stats.mean()
    }

    pub fn sigma(&self) -> Option<f64> {
        self.stats.std_dev()
    }

    /// `(lower, upper)` at `k` standard deviations from VWAP.
    pub fn band(&self, k: f64) -> Option<(f64, f64)> {
        let (v, s) = (self.vwap()?, self.sigma()?);
        Some((v - k * s, v + k * s))
    }

    pub fn reset(&mut self) {
        self.stats = WeightedStats::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_mean_and_bands() {
        let mut v = VwapBands::new();
        assert_eq!(v.vwap(), None);
        v.update(100.0, 1.0);
        v.update(110.0, 3.0);
        assert_eq!(v.vwap(), Some(107.5));
        // E[x^2] - mean^2 = (10000 + 3 * 12100) / 4 - 107.5^2 = 18.75
        let s = v.sigma().unwrap();
        assert!((s - 18.75f64.sqrt()).abs() < 1e-12);
        let (lo, hi) = v.band(2.0).unwrap();
        assert!((hi - lo - 4.0 * s).abs() < 1e-9);
        v.update(1.0, 0.0); // zero volume is ignored
        assert_eq!(v.vwap(), Some(107.5));
        v.reset();
        assert_eq!(v.vwap(), None);
    }

    #[test]
    fn stable_with_large_prices() {
        // Rs 50,000 in paise with tiny moves: the naive E[x^2] - mean^2
        // formula loses most of its digits here.
        let mut v = VwapBands::new();
        for i in 0..100_000u64 {
            v.update(5_000_000.0 + (i % 2) as f64, 1000.0);
        }
        assert!((v.sigma().unwrap() - 0.5).abs() < 1e-9);
    }
}
