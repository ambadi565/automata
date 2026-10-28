//! Synthetic 5-minute bars for exercising the pipeline without market data.
//!
//! **Not evidence of anything.** In `regimes` mode the generator plants
//! trend days (autocorrelated drift), range days (mean reversion to the
//! open) and volatile days, so the strategies have something to find; a
//! strategy passing the gate on this data says only that the plumbing works.
//! `random_walk` mode has no structure at all: nothing should pass on it
//! after costs, which makes it a useful check that the backtester does not
//! invent an edge.

use std::path::Path;

use anyhow::Result;
use chrono::{Datelike, NaiveDate, NaiveTime, TimeDelta, Weekday};
use saarthi_core::{Bar, MarketClock, Price, Symbol, Tf};
use saarthi_store::DataLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SynthMode {
    Regimes,
    RandomWalk,
}

/// SplitMix64: small, fast, deterministic across platforms.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal (Box–Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Weekdays from `start`, `n` of them.
pub fn weekdays(start: NaiveDate, n: usize) -> Vec<NaiveDate> {
    let mut out = Vec::with_capacity(n);
    let mut d = start;
    while out.len() < n {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            out.push(d);
        }
        d = d.succ_opt().expect("date in range");
    }
    out
}

const BARS_PER_DAY: i64 = 75; // 09:15 .. 15:25 start times

/// One symbol's 5-minute bars over `days`.
pub fn generate(rng: &mut Rng, days: &[NaiveDate], mode: SynthMode) -> Vec<Bar> {
    let open_t = NaiveTime::from_hms_opt(9, 15, 0).expect("valid");
    let mut price = 200.0 + rng.uniform() * 2800.0;
    let base_vol = 0.0012 + rng.uniform() * 0.0012; // per 5-min bar
    let base_volume = 20_000.0 + rng.uniform() * 200_000.0;
    let tick = 0.05;
    let mut out = Vec::with_capacity(days.len() * BARS_PER_DAY as usize);
    for &day in days {
        // Overnight gap.
        price *= (rng.normal() * 0.006).exp();
        let day_open = price;
        let r = rng.uniform();
        let (drift, kappa, vol_mult, vol_boost) = match mode {
            SynthMode::RandomWalk => (0.0, 0.0, 1.0, 1.0),
            SynthMode::Regimes if r < 0.35 => {
                let dir = if rng.uniform() < 0.5 { -1.0 } else { 1.0 };
                (dir * base_vol * (0.25 + 0.25 * rng.uniform()), 0.0, 1.0, 1.6)
            }
            SynthMode::Regimes if r < 0.8 => (0.0, 0.12, 0.9, 1.0),
            SynthMode::Regimes => (0.0, 0.0, 1.8, 1.3),
        };
        let start = MarketClock::at(day, open_t);
        for i in 0..BARS_PER_DAY {
            // U-shaped intraday volatility and volume.
            let x = i as f64 / (BARS_PER_DAY - 1) as f64;
            let u = 1.0 + 1.2 * (2.0 * x - 1.0).powi(2);
            let sigma = base_vol * vol_mult * u.sqrt() / 5f64.sqrt();
            let open = price;
            let (mut hi, mut lo) = (open, open);
            for _ in 0..5 {
                let pull = -kappa * (price / day_open).ln() / 5.0;
                price *= (drift / 5.0 + pull + sigma * rng.normal()).exp();
                hi = hi.max(price);
                lo = lo.min(price);
            }
            let q = |v: f64| Price::from_rupees_f64((v / tick).round() * tick).expect("finite price");
            let boost = if i < 12 { vol_boost } else { 1.0 };
            let volume = base_volume * u * boost * (0.4 * rng.normal()).exp();
            let (o, c) = (q(open), q(price));
            out.push(Bar {
                ts: start + TimeDelta::minutes(5 * i),
                open: o,
                high: q(hi).max(o).max(c),
                low: q(lo).min(o).min(c),
                close: c,
                volume: volume.max(1.0) as u64,
            });
        }
    }
    out
}

/// Writes `symbols` synthetic series as `data/candles/5m/SYNnnn-EQ.parquet`
/// and returns their symbols.
pub fn write(data_dir: &Path, n_symbols: usize, days: &[NaiveDate], seed: u64, mode: SynthMode) -> Result<Vec<Symbol>> {
    let layout = DataLayout::new(data_dir);
    let mut rng = Rng::new(seed);
    let mut syms = Vec::with_capacity(n_symbols);
    for k in 0..n_symbols {
        let sym = Symbol::new(format!("SYN{:03}-EQ", k + 1));
        let bars = generate(&mut rng, days, mode);
        saarthi_store::parquet::write_bars(&layout.candles(Tf::M5, &sym), &bars)?;
        syms.push(sym);
    }
    Ok(syms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_consistent() {
        let days = weekdays(NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(), 3);
        let a = generate(&mut Rng::new(7), &days, SynthMode::Regimes);
        let b = generate(&mut Rng::new(7), &days, SynthMode::Regimes);
        assert_eq!(a, b);
        assert_eq!(a.len(), 3 * 75);
        assert!(a.iter().all(Bar::is_consistent));
        assert!(
            a.iter().all(|b| b.open.0 % 5 == 0 && b.close.0 % 5 == 0),
            "on the 5-paise tick"
        );
        assert_eq!(days[0].weekday(), Weekday::Mon);
    }
}
