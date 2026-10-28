//! VWAP reversion: in a range-bound tape, fade a stretch beyond `k` σ from the
//! session VWAP once price closes back inside the band. Target is the VWAP;
//! stop is beyond the extreme of the stretch.

use chrono::TimeDelta;
use saarthi_bars::Snapshot;
use saarthi_core::{Price, Side};

use crate::params::{ParamError, ParamSet, ParamSpec, resolve};
use crate::{BarCtx, DayCounter, Exit, Signal, Strategy, signal};

#[derive(Debug, Clone)]
pub struct VwapRevert {
    band_k: f64,
    atr_stop: f64,
    max_trend: f64,
    rsi_band: f64,
    first_entry: TimeDelta,
    max_trades: u32,
    day: DayCounter,
    /// Extreme low/high of the current stretch outside the band.
    stretch_low: Option<Price>,
    stretch_high: Option<Price>,
    /// The stretch was outside the band on the previous bar.
    was_below: bool,
    was_above: bool,
}

impl VwapRevert {
    pub const ID: &'static str = "vwap_revert";

    pub fn specs() -> Vec<ParamSpec> {
        vec![
            // Band width in volume-weighted σ around VWAP.
            ParamSpec::new("band_k", 2.0, 0.5, 5.0, &[1.5, 2.0, 2.5]),
            // Stop beyond the stretch extreme, in ATRs.
            ParamSpec::new("atr_stop", 0.5, 0.0, 5.0, &[0.25, 0.5, 1.0]),
            // Range filter: |EMA fast - EMA slow| must be under this many ATRs.
            ParamSpec::new("max_trend", 0.5, 0.0, 10.0, &[0.3, 0.5, 0.8]),
            // Momentum must still be stretched on the re-entry bar: RSI at
            // most 50 - x for longs, at least 50 + x for shorts.
            ParamSpec::new("rsi_band", 10.0, 0.0, 50.0, &[]),
            // Let VWAP settle before fading it.
            ParamSpec::new("first_entry_min", 30.0, 0.0, 300.0, &[]),
            ParamSpec::new("max_trades_per_day", 2.0, 1.0, 10.0, &[]),
        ]
    }

    pub fn new(p: &ParamSet) -> Result<Self, ParamError> {
        let p = resolve(Self::ID, &Self::specs(), p)?;
        let g = |k| p.get(k).expect("resolved");
        Ok(VwapRevert {
            band_k: g("band_k"),
            atr_stop: g("atr_stop"),
            max_trend: g("max_trend"),
            rsi_band: g("rsi_band"),
            first_entry: TimeDelta::minutes(g("first_entry_min") as i64),
            max_trades: g("max_trades_per_day") as u32,
            day: DayCounter::default(),
            stretch_low: None,
            stretch_high: None,
            was_below: false,
            was_above: false,
        })
    }

    fn reset_stretch(&mut self) {
        self.stretch_low = None;
        self.stretch_high = None;
        self.was_below = false;
        self.was_above = false;
    }
}

impl Strategy for VwapRevert {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn on_bar(&mut self, s: &Snapshot, ctx: &BarCtx) -> Option<Signal> {
        if self.day.roll(ctx.now) {
            self.reset_stretch();
        }
        let (vwap, sigma) = (s.vwap?, s.vwap_sigma?);
        let (atr, fast, slow, rsi) = (s.atr?, s.ema_fast?, s.ema_slow?, s.rsi?);
        let b = ctx.bar;
        let v = vwap.0 as f64;
        let lower = v - self.band_k * sigma;
        let upper = v + self.band_k * sigma;
        let c = b.close.0 as f64;

        // Track the stretch: any bar trading outside the band extends it.
        if (b.low.0 as f64) < lower {
            self.stretch_low = Some(self.stretch_low.map_or(b.low, |l| l.min(b.low)));
        }
        if (b.high.0 as f64) > upper {
            self.stretch_high = Some(self.stretch_high.map_or(b.high, |h| h.max(b.high)));
        }
        let below = c < lower;
        let above = c > upper;
        let (was_below, was_above) = (self.was_below, self.was_above);
        self.was_below = below;
        self.was_above = above;
        // Back near VWAP: the stretch is over (for the next setup; this
        // bar's re-entry still uses it).
        let (stretch_low, stretch_high) = (self.stretch_low, self.stretch_high);
        if (c - v).abs() < 0.5 * self.band_k * sigma {
            self.stretch_low = None;
            self.stretch_high = None;
        }

        if ctx.position.is_some()
            || !ctx.entries_allowed
            || ctx.since_open < self.first_entry
            || self.day.count >= self.max_trades
            || atr <= 0.0
            || sigma <= 0.0
            || (fast - slow).abs() > self.max_trend * atr
        {
            return None;
        }
        let (side, extreme) = if was_below && !below && rsi <= 50.0 - self.rsi_band {
            (Side::Buy, stretch_low?)
        } else if was_above && !above && rsi >= 50.0 + self.rsi_band {
            (Side::Sell, stretch_high?)
        } else {
            return None;
        };
        if !ctx.direction.allows(side) {
            return None;
        }
        let stop = match side {
            Side::Buy => extreme.0 as f64 - self.atr_stop * atr,
            Side::Sell => extreme.0 as f64 + self.atr_stop * atr,
        };
        let sig = signal(
            s,
            side,
            b.close,
            stop,
            Some(v),
            0.5,
            format!("re-entry inside {:.1}σ band, VWAP {vwap}, RSI {rsi:.0}", self.band_k),
        )?;
        self.day.count += 1;
        self.reset_stretch();
        Some(sig)
    }

    /// The range broke into a trend: get out rather than wait for the stop.
    fn manage(&mut self, s: &Snapshot, _ctx: &BarCtx) -> Exit {
        match (s.ema_fast, s.ema_slow, s.atr) {
            (Some(f), Some(sl), Some(a)) if (f - sl).abs() > 2.0 * self.max_trend.max(0.25) * a => {
                Exit::Close("regime turned to trend".into())
            }
            _ => Exit::Hold,
        }
    }
}
