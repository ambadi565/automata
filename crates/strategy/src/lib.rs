//! Trading strategies.
//!
//! A [`Strategy`] reads one symbol's indicator [`Snapshot`] at every completed
//! bar and may emit a [`Signal`]: side, entry, stop and optional target. It
//! never sizes, places or manages orders; the risk guard sizes the signal and
//! the OMS (or the backtester) executes it. The same strategy code runs in
//! backtest, paper and live.
//!
//! Strategies are built by name from a [`ParamSet`] through the
//! [`registry`], which also publishes each strategy's default parameters and
//! the grid the walk-forward optimizer searches.
//!
//! Prices in snapshots are paise: observed levels are exact
//! [`saarthi_core::Price`]s, computed indicators are unrounded `f64` paise.

mod momentum;
mod orb;
mod params;
pub mod registry;
mod vwap_revert;

use chrono::TimeDelta;
use saarthi_bars::Snapshot;
use saarthi_core::{Bar, Price, Side, Ts};
use serde::{Deserialize, Serialize};

pub use momentum::Momentum;
pub use orb::Orb;
pub use params::{ParamError, ParamGrid, ParamSet, ParamSpec};
pub use vwap_revert::VwapRevert;

/// Which way a strategy may trade a symbol (the plan's `direction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    LongOnly,
    ShortOnly,
    #[default]
    Both,
}

impl Direction {
    pub fn allows(self, side: Side) -> bool {
        match self {
            Direction::Both => true,
            Direction::LongOnly => side == Side::Buy,
            Direction::ShortOnly => side == Side::Sell,
        }
    }
}

/// What the strategy knows about the moment and its own position, besides
/// the snapshot.
#[derive(Debug, Clone, Copy)]
pub struct BarCtx {
    /// The bar that just completed (on the indicator timeframe).
    pub bar: Bar,
    /// The bar's close time, which is "now".
    pub now: Ts,
    /// Time since the session opened (09:15) at `now`.
    pub since_open: TimeDelta,
    /// New entries are allowed (before the no-new-entries cut-off, not
    /// halted). Strategies still update their state when this is false.
    pub entries_allowed: bool,
    /// The symbol's current position from this strategy, if any.
    pub position: Option<Side>,
    pub direction: Direction,
}

/// A trade idea. The risk guard decides whether and how big.
#[derive(Debug, Clone, PartialEq)]
pub struct Signal {
    pub token: String,
    pub side: Side,
    /// Reference entry price (the signal bar's close). Fills will differ.
    pub entry: Price,
    /// Protective stop. Always on the losing side of `entry`.
    pub stop: Price,
    pub target: Option<Price>,
    pub confidence: f32,
    pub reason: String,
}

impl Signal {
    /// Per-share risk, entry to stop, in paise (always positive for a valid
    /// signal).
    pub fn risk_per_share(&self) -> i64 {
        match self.side {
            Side::Buy => self.entry.0 - self.stop.0,
            Side::Sell => self.stop.0 - self.entry.0,
        }
    }

    /// Stop on the losing side and target (if any) on the winning side.
    pub fn is_well_formed(&self) -> bool {
        let target_ok = self.target.is_none_or(|t| match self.side {
            Side::Buy => t > self.entry,
            Side::Sell => t < self.entry,
        });
        self.entry.0 > 0 && self.stop.0 > 0 && self.risk_per_share() > 0 && target_ok
    }
}

/// What a strategy wants done with an open position at a bar close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Hold,
    /// Exit at market, with a reason for the trade log.
    Close(String),
}

/// One strategy instance trades one symbol. Build one per symbol.
pub trait Strategy: Send {
    fn id(&self) -> &'static str;

    /// Called once per completed bar, in time order. Returns an entry signal
    /// only when flat (`ctx.position` is `None`) and entries are allowed.
    fn on_bar(&mut self, s: &Snapshot, ctx: &BarCtx) -> Option<Signal>;

    /// Called at each bar close while a position from this strategy is open,
    /// after [`Strategy::on_bar`]. Stops and targets are resting orders; this
    /// is for discretionary exits (e.g. trend broken).
    fn manage(&mut self, _s: &Snapshot, _ctx: &BarCtx) -> Exit {
        Exit::Hold
    }
}

/// Rounds an `f64` paise level to a whole paisa.
pub(crate) fn px(v: f64) -> Price {
    Price(v.round() as i64)
}

/// Builds a signal and drops it if it is malformed (e.g. a zero-width stop).
pub(crate) fn signal(
    s: &Snapshot,
    side: Side,
    entry: Price,
    stop: f64,
    target: Option<f64>,
    confidence: f32,
    reason: String,
) -> Option<Signal> {
    let sig = Signal {
        token: s.token.clone(),
        side,
        entry,
        stop: px(stop),
        target: target.map(px),
        confidence,
        reason,
    };
    sig.is_well_formed().then_some(sig)
}

/// Tracks the trading day so per-day counters reset.
#[derive(Debug, Clone, Default)]
pub(crate) struct DayCounter {
    day: Option<chrono::NaiveDate>,
    pub(crate) count: u32,
}

impl DayCounter {
    /// Resets on a new IST day; returns true if it did.
    pub(crate) fn roll(&mut self, now: Ts) -> bool {
        let d = saarthi_core::MarketClock::ist_date(now);
        if self.day != Some(d) {
            self.day = Some(d);
            self.count = 0;
            true
        } else {
            false
        }
    }
}
