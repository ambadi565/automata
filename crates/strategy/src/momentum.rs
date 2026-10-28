//! Trend momentum: EMA fast above slow (by a margin) and price on the right
//! side of VWAP, entered on a pullback that tags the fast EMA and closes back
//! in the trend's direction. Exits on stop, target, or a close through the
//! slow EMA.

use chrono::TimeDelta;
use saarthi_bars::Snapshot;
use saarthi_core::Side;

use crate::params::{ParamError, ParamSet, ParamSpec, resolve};
use crate::{BarCtx, DayCounter, Exit, Signal, Strategy, signal};

#[derive(Debug, Clone)]
pub struct Momentum {
    min_sep: f64,
    atr_stop: f64,
    rr: f64,
    touch: f64,
    first_entry: TimeDelta,
    max_trades: u32,
    day: DayCounter,
}

impl Momentum {
    pub const ID: &'static str = "momentum";

    pub fn specs() -> Vec<ParamSpec> {
        vec![
            // Trend filter: EMA fast - EMA slow of at least this many ATRs.
            ParamSpec::new("min_sep", 0.5, 0.0, 10.0, &[0.3, 0.5, 0.8]),
            // Stop below the pullback low / slow EMA, in ATRs.
            ParamSpec::new("atr_stop", 0.5, 0.0, 5.0, &[0.25, 0.5, 1.0]),
            ParamSpec::new("rr", 2.0, 0.5, 10.0, &[1.5, 2.0, 3.0]),
            // How close (in ATRs) the bar's low must come to the fast EMA.
            ParamSpec::new("touch", 0.1, 0.0, 2.0, &[]),
            ParamSpec::new("first_entry_min", 30.0, 0.0, 300.0, &[]),
            ParamSpec::new("max_trades_per_day", 2.0, 1.0, 10.0, &[]),
        ]
    }

    pub fn new(p: &ParamSet) -> Result<Self, ParamError> {
        let p = resolve(Self::ID, &Self::specs(), p)?;
        let g = |k| p.get(k).expect("resolved");
        Ok(Momentum {
            min_sep: g("min_sep"),
            atr_stop: g("atr_stop"),
            rr: g("rr"),
            touch: g("touch"),
            first_entry: TimeDelta::minutes(g("first_entry_min") as i64),
            max_trades: g("max_trades_per_day") as u32,
            day: DayCounter::default(),
        })
    }
}

impl Strategy for Momentum {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn on_bar(&mut self, s: &Snapshot, ctx: &BarCtx) -> Option<Signal> {
        self.day.roll(ctx.now);
        let (fast, slow, atr, vwap) = (s.ema_fast?, s.ema_slow?, s.atr?, s.vwap?);
        if ctx.position.is_some()
            || !ctx.entries_allowed
            || ctx.since_open < self.first_entry
            || self.day.count >= self.max_trades
            || atr <= 0.0
        {
            return None;
        }
        let b = ctx.bar;
        let (o, h, l, c) = (b.open.0 as f64, b.high.0 as f64, b.low.0 as f64, b.close.0 as f64);
        let sep = (fast - slow) / atr;
        let tol = self.touch * atr;
        let side = if sep >= self.min_sep && c > vwap.0 as f64 && l <= fast + tol && c > fast && c > o {
            Side::Buy
        } else if -sep >= self.min_sep && c < vwap.0 as f64 && h >= fast - tol && c < fast && c < o {
            Side::Sell
        } else {
            return None;
        };
        if !ctx.direction.allows(side) {
            return None;
        }
        let (stop, risk) = match side {
            Side::Buy => {
                let st = l.min(slow) - self.atr_stop * atr;
                (st, c - st)
            }
            Side::Sell => {
                let st = h.max(slow) + self.atr_stop * atr;
                (st, st - c)
            }
        };
        let target = match side {
            Side::Buy => c + self.rr * risk,
            Side::Sell => c - self.rr * risk,
        };
        let sig = signal(
            s,
            side,
            b.close,
            stop,
            Some(target),
            (sep.abs() / 2.0).min(1.0) as f32,
            format!("pullback to EMA in trend, sep {sep:.2} ATR"),
        )?;
        self.day.count += 1;
        Some(sig)
    }

    fn manage(&mut self, s: &Snapshot, ctx: &BarCtx) -> Exit {
        let Some(slow) = s.ema_slow else { return Exit::Hold };
        let c = ctx.bar.close.0 as f64;
        match ctx.position {
            Some(Side::Buy) if c < slow => Exit::Close("closed below slow EMA".into()),
            Some(Side::Sell) if c > slow => Exit::Close("closed above slow EMA".into()),
            _ => Exit::Hold,
        }
    }
}
