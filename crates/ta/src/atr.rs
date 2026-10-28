use crate::{Hlc, Wilder};

/// Average true range with Wilder smoothing.
///
/// The first bar only provides the previous close, so the first value comes
/// on bar `period + 1` as the average of the first `period` true ranges, as in
/// TA-Lib.
#[derive(Debug, Clone)]
pub struct Atr {
    period: usize,
    prev_close: Option<f64>,
    rma: Wilder,
}

impl Atr {
    pub fn new(period: usize) -> Self {
        Atr {
            period,
            prev_close: None,
            rma: Wilder::new(period),
        }
    }

    pub fn update(&mut self, bar: Hlc) -> Option<f64> {
        let out = self.prev_close.and_then(|pc| {
            let tr = (bar.high - bar.low)
                .max((bar.high - pc).abs())
                .max((bar.low - pc).abs());
            self.rma.update(tr)
        });
        self.prev_close = Some(bar.close);
        out
    }

    pub fn value(&self) -> Option<f64> {
        self.rma.value()
    }

    pub fn reset(&mut self) {
        *self = Atr::new(self.period);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn true_range_uses_gaps() {
        let mut a = Atr::new(2);
        let hlc = |h, l, c| Hlc {
            high: h,
            low: l,
            close: c,
        };
        assert_eq!(a.update(hlc(10.0, 9.0, 10.0)), None);
        // Gap up: TR = 15 - 10 = 5, not 15 - 14 = 1.
        assert_eq!(a.update(hlc(15.0, 14.0, 15.0)), None);
        // TR = 2; seed = (5 + 2) / 2
        assert_eq!(a.update(hlc(16.0, 14.0, 15.0)), Some(3.5));
        // Wilder: (3.5 * 1 + 1) / 2
        assert_eq!(a.update(hlc(15.5, 14.5, 15.0)), Some(2.25));
    }
}
