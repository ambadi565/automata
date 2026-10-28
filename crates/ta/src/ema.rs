/// Exponential moving average with `alpha = 2 / (period + 1)`, seeded with the
/// simple average of the first `period` inputs (TA-Lib's convention).
#[derive(Debug, Clone)]
pub struct Ema {
    period: usize,
    alpha: f64,
    count: usize,
    seed_sum: f64,
    value: Option<f64>,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "EMA period must be positive");
        Ema {
            period,
            alpha: 2.0 / (period as f64 + 1.0),
            count: 0,
            seed_sum: 0.0,
            value: None,
        }
    }

    pub fn period(&self) -> usize {
        self.period
    }

    pub fn update(&mut self, x: f64) -> Option<f64> {
        self.value = match self.value {
            Some(v) => Some(v + self.alpha * (x - v)),
            None => {
                self.count += 1;
                self.seed_sum += x;
                (self.count == self.period).then(|| self.seed_sum / self.period as f64)
            }
        };
        self.value
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn reset(&mut self) {
        *self = Ema::new(self.period);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_with_sma_then_smooths() {
        let mut e = Ema::new(3);
        assert_eq!(e.update(1.0), None);
        assert_eq!(e.update(2.0), None);
        assert_eq!(e.update(3.0), Some(2.0));
        // alpha = 0.5: 2 + 0.5 * (6 - 2) = 4
        assert_eq!(e.update(6.0), Some(4.0));
        e.reset();
        assert_eq!(e.value(), None);
    }
}
