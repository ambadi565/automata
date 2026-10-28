//! Incremental technical indicators.
//!
//! Every indicator is updated one observation at a time in O(1) and never
//! recomputes history, so the engine can call it on every bar or tick. Inputs
//! are `f64` in whatever unit the caller uses (the engine passes paise), and
//! outputs are in the same unit.
//!
//! Warm-up conventions match TA-Lib, which the tests use as the reference:
//! EMA is seeded with the simple average of its first `period` inputs, and ATR
//! and RSI use Wilder smoothing seeded with the simple average of their first
//! `period` values. An indicator returns `None` until it has enough data.

mod atr;
mod ema;
mod rsi;
mod volume;
mod vwap;

pub use atr::Atr;
pub use ema::Ema;
pub use rsi::Rsi;
pub use volume::VolumeZScore;
pub use vwap::{VwapBands, WeightedStats};

/// One OHLC observation for indicators that need a range (ATR).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hlc {
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// Wilder's running moving average (RMA), seeded with a simple average.
#[derive(Debug, Clone)]
pub(crate) struct Wilder {
    period: usize,
    count: usize,
    seed_sum: f64,
    value: Option<f64>,
}

impl Wilder {
    pub(crate) fn new(period: usize) -> Self {
        assert!(period > 0, "period must be positive");
        Wilder {
            period,
            count: 0,
            seed_sum: 0.0,
            value: None,
        }
    }

    pub(crate) fn update(&mut self, x: f64) -> Option<f64> {
        let n = self.period as f64;
        self.value = match self.value {
            Some(v) => Some((v * (n - 1.0) + x) / n),
            None => {
                self.count += 1;
                self.seed_sum += x;
                (self.count == self.period).then(|| self.seed_sum / n)
            }
        };
        self.value
    }

    pub(crate) fn value(&self) -> Option<f64> {
        self.value
    }
}
