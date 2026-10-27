use crate::Wilder;

/// Wilder's relative strength index on closes.
///
/// The first value comes after `period` price changes (input `period + 1`),
/// as in TA-Lib. A series with no movement at all reads 50, the neutral value,
/// where TA-Lib would return 0.
#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    prev: Option<f64>,
    gain: Wilder,
    loss: Wilder,
    value: Option<f64>,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        Rsi {
            period,
            prev: None,
            gain: Wilder::new(period),
            loss: Wilder::new(period),
            value: None,
        }
    }

    pub fn update(&mut self, close: f64) -> Option<f64> {
        if let Some(prev) = self.prev {
            let d = close - prev;
            let g = self.gain.update(d.max(0.0));
            let l = self.loss.update((-d).max(0.0));
            if let (Some(g), Some(l)) = (g, l) {
                let total = g + l;
                self.value = Some(if total > 0.0 { 100.0 * g / total } else { 50.0 });
            }
        }
        self.prev = Some(close);
        self.value
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn reset(&mut self) {
        *self = Rsi::new(self.period);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_and_flat_series() {
        let mut up = Rsi::new(3);
        let mut flat = Rsi::new(3);
        let mut last = None;
        for i in 0..10 {
            last = up.update(i as f64);
            flat.update(5.0);
        }
        assert_eq!(last, Some(100.0));
        assert_eq!(flat.value(), Some(50.0));
        let mut r = Rsi::new(3);
        assert_eq!(r.update(1.0), None);
        assert_eq!(r.update(2.0), None);
        assert_eq!(r.update(1.0), None);
        // gains 1,0,0 losses 0,1,0 -> 50
        assert!((r.update(1.0).unwrap() - 50.0).abs() < 1e-9);
    }
}
