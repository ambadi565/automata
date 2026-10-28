//! Opening-range breakout: the first bar to close beyond the 09:15–09:30
//! range, on above-normal volume, early in the day. ATR stop, R-multiple
//! target, at most one trade per symbol per day.

use chrono::TimeDelta;
use saarthi_bars::Snapshot;
use saarthi_core::{Price, Side};

use crate::params::{ParamError, ParamSet, ParamSpec, resolve};
use crate::{BarCtx, DayCounter, Signal, Strategy, signal};

#[derive(Debug, Clone)]
pub struct Orb {
    atr_stop: f64,
    rr: f64,
    min_vol_z: f64,
    last_entry: TimeDelta,
    max_range_pct: f64,
    day: DayCounter,
    prev_close: Option<Price>,
}

impl Orb {
    pub const ID: &'static str = "orb";

    pub fn specs() -> Vec<ParamSpec> {
        vec![
            // Stop distance in ATRs from the entry.
            ParamSpec::new("atr_stop", 1.5, 0.25, 5.0, &[1.0, 1.5, 2.0]),
            // Target in multiples of the risk (R).
            ParamSpec::new("rr", 2.0, 0.5, 10.0, &[1.5, 2.0, 3.0]),
            // Breakout bar's volume z-score must be at least this.
            ParamSpec::new("min_vol_z", 1.0, -5.0, 10.0, &[0.5, 1.0, 1.5]),
            // No new breakouts after this many minutes from the open.
            ParamSpec::new("last_entry_min", 120.0, 15.0, 330.0, &[]),
            // Skip days whose opening range is wider than this % of price.
            ParamSpec::new("max_range_pct", 2.5, 0.1, 20.0, &[]),
        ]
    }

    pub fn new(p: &ParamSet) -> Result<Self, ParamError> {
        let p = resolve(Self::ID, &Self::specs(), p)?;
        let g = |k| p.get(k).expect("resolved");
        Ok(Orb {
            atr_stop: g("atr_stop"),
            rr: g("rr"),
            min_vol_z: g("min_vol_z"),
            last_entry: TimeDelta::minutes(g("last_entry_min") as i64),
            max_range_pct: g("max_range_pct"),
            day: DayCounter::default(),
            prev_close: None,
        })
    }
}

impl Strategy for Orb {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn on_bar(&mut self, s: &Snapshot, ctx: &BarCtx) -> Option<Signal> {
        if self.day.roll(ctx.now) {
            self.prev_close = None;
        }
        let close = ctx.bar.close;
        let prev = self.prev_close.replace(close);
        let (hi, lo) = (s.or_high?, s.or_low?);
        let atr = s.atr?;
        let vol_z = s.vol_z?;
        if self.day.count > 0 || ctx.position.is_some() || !ctx.entries_allowed || ctx.since_open > self.last_entry {
            return None;
        }
        let range_pct = (hi.0 - lo.0) as f64 * 100.0 / close.0 as f64;
        if range_pct > self.max_range_pct || vol_z < self.min_vol_z || atr <= 0.0 {
            return None;
        }
        // A fresh cross: the previous bar closed inside the range.
        let prev = prev?;
        let side = if close > hi && prev <= hi {
            Side::Buy
        } else if close < lo && prev >= lo {
            Side::Sell
        } else {
            return None;
        };
        if !ctx.direction.allows(side) {
            return None;
        }
        let e = close.0 as f64;
        let risk = self.atr_stop * atr;
        let (stop, target) = match side {
            Side::Buy => (e - risk, e + self.rr * risk),
            Side::Sell => (e + risk, e - self.rr * risk),
        };
        let sig = signal(
            s,
            side,
            close,
            stop,
            Some(target),
            vol_z.clamp(0.0, 3.0) as f32 / 3.0,
            format!("OR {hi}-{lo} break, vol_z {vol_z:.1}"),
        )?;
        self.day.count += 1;
        Some(sig)
    }
}
