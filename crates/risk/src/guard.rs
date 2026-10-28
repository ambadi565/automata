//! The risk guard: pre-trade checks, the daily P&L monitor and the kill
//! switch. Pure and synchronous, so every limit can be tested with a breach
//! scenario and the engine calls it on the hot path without awaiting.

use std::collections::HashMap;

use chrono::{NaiveDate, TimeDelta};
use saarthi_core::{MarketClock, Phase, Price, Qty, Side, Ts};
use serde::{Deserialize, Serialize};

use crate::limits::{RiskLimits, pct_of};

/// Why an entry was refused. `code()` is stable for logs and the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rejection {
    Halted(HaltReason),
    Phase(String),
    FeedUnhealthy,
    DailyLossHit,
    ProfitLocked,
    Cooldown { until: Ts },
    MaxTradesPerDay,
    MaxOpenPositions,
    SymbolBusy,
    Exposure,
    BadSignal(String),
    MinPrice,
    SizeZero,
    PriceBand(String),
    Duplicate,
    NoCapital,
}

impl Rejection {
    pub fn code(&self) -> &'static str {
        match self {
            Rejection::Halted(_) => "halted",
            Rejection::Phase(_) => "phase",
            Rejection::FeedUnhealthy => "feed_unhealthy",
            Rejection::DailyLossHit => "daily_loss",
            Rejection::ProfitLocked => "profit_locked",
            Rejection::Cooldown { .. } => "cooldown",
            Rejection::MaxTradesPerDay => "max_trades_per_day",
            Rejection::MaxOpenPositions => "max_open_positions",
            Rejection::SymbolBusy => "symbol_busy",
            Rejection::Exposure => "exposure",
            Rejection::BadSignal(_) => "bad_signal",
            Rejection::MinPrice => "min_price",
            Rejection::SizeZero => "size_zero",
            Rejection::PriceBand(_) => "price_band",
            Rejection::Duplicate => "duplicate",
            Rejection::NoCapital => "no_capital",
        }
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::Halted(r) => write!(f, "halted: {r}"),
            Rejection::Phase(p) => write!(f, "phase {p} does not allow entries"),
            Rejection::Cooldown { until } => write!(f, "consecutive-loss cooldown until {until}"),
            Rejection::BadSignal(m) | Rejection::PriceBand(m) => write!(f, "{}: {m}", self.code()),
            other => f.write_str(other.code()),
        }
    }
}

/// Why trading is halted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HaltReason {
    /// The daily loss limit was hit. Clears on the next trading day.
    DailyLoss,
    /// Kill switch: Telegram `/halt`, the HALT file or an operator. Stays
    /// until resumed.
    Manual(String),
    /// The engine found a state it cannot trust (reconciliation mismatch,
    /// auth failure, square-off failure). Stays until resumed.
    Fault(String),
}

impl std::fmt::Display for HaltReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HaltReason::DailyLoss => f.write_str("daily loss limit hit"),
            HaltReason::Manual(m) => write!(f, "manual halt ({m})"),
            HaltReason::Fault(m) => write!(f, "fault ({m})"),
        }
    }
}

/// Exchange price band (circuit limits) for a symbol, when known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceBand {
    pub lower: Price,
    pub upper: Price,
}

/// One proposed entry, before sizing.
#[derive(Debug, Clone)]
pub struct EntryProposal<'a> {
    pub now: Ts,
    pub token: &'a str,
    pub strategy: &'a str,
    pub side: Side,
    /// Reference entry price (the signal's).
    pub entry: Price,
    pub stop: Price,
    /// Last traded price, for the deviation check.
    pub ltp: Option<Price>,
    pub band: Option<PriceBand>,
    /// Extra scale on risk per trade in [0, 1] (the plan's multiplier, M4).
    pub risk_scale: f64,
}

/// What the engine knows about its book and surroundings at check time.
#[derive(Debug, Clone, Copy)]
pub struct Book {
    pub phase: Phase,
    /// Feed connected and fresh.
    pub feed_ok: bool,
    /// Positions open or entries working (one per symbol).
    pub open_positions: usize,
    /// Σ |qty| × price over open positions and working entries.
    pub gross_exposure: Price,
    /// A trade on this symbol is already open or working.
    pub symbol_busy: bool,
}

/// An approved entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Approval {
    pub qty: Qty,
    /// Planned loss at the stop, qty × (entry − stop).
    pub risk: Price,
}

/// What the P&L monitor wants done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breach {
    /// Daily loss limit hit: cancel everything, flatten, halt for the day.
    FlattenAndHalt,
    /// Profit lock hit: no new entries today.
    ProfitLocked,
}

/// Everything the guard must remember across a restart.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RiskState {
    pub day: Option<NaiveDate>,
    /// Capital base for today's percentages.
    pub capital: Price,
    pub halted: Option<HaltReason>,
    pub profit_locked: bool,
    pub trades_today: u32,
    pub consecutive_losses: u32,
    pub cooldown_until: Option<Ts>,
    /// Worst day P&L seen today (for the report).
    pub worst_pnl: Price,
    /// (token, strategy, side) → last accepted entry time.
    #[serde(default)]
    pub recent: Vec<(String, String, bool, Ts)>,
}

pub struct RiskGuard {
    limits: RiskLimits,
    state: RiskState,
    recent: HashMap<(String, String, bool), Ts>,
}

impl RiskGuard {
    pub fn new(limits: RiskLimits) -> Self {
        RiskGuard {
            limits,
            state: RiskState::default(),
            recent: HashMap::new(),
        }
    }

    /// Restores state persisted before a restart.
    pub fn restore(limits: RiskLimits, state: RiskState) -> Self {
        let recent = state
            .recent
            .iter()
            .map(|(t, s, b, ts)| ((t.clone(), s.clone(), *b), *ts))
            .collect();
        RiskGuard { limits, state, recent }
    }

    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    /// The state to persist.
    pub fn state(&self) -> RiskState {
        let mut s = self.state.clone();
        s.recent = self
            .recent
            .iter()
            .map(|((t, st, b), ts)| (t.clone(), st.clone(), *b, *ts))
            .collect();
        s.recent.sort();
        s
    }

    /// Starts (or continues) trading day `day`. `funds` is the broker's net
    /// funds, used as the capital base when `risk.toml` does not fix one.
    /// Calling it again on the same day keeps the day's counters (restart).
    pub fn begin_day(&mut self, day: NaiveDate, funds: Price) {
        if self.state.day == Some(day) {
            return;
        }
        let manual = match &self.state.halted {
            Some(HaltReason::DailyLoss) | None => None,
            other => other.clone(),
        };
        self.state = RiskState {
            day: Some(day),
            capital: self.limits.fixed_capital().unwrap_or(funds),
            halted: manual,
            // Consecutive losses carry over; the cooldown is time-based.
            consecutive_losses: self.state.consecutive_losses,
            ..RiskState::default()
        };
        self.recent.clear();
    }

    pub fn capital(&self) -> Price {
        self.state.capital
    }

    pub fn halted(&self) -> Option<&HaltReason> {
        self.state.halted.as_ref()
    }

    pub fn halt(&mut self, reason: HaltReason) {
        // A daily-loss halt is never downgraded to a resumable one.
        if self.state.halted != Some(HaltReason::DailyLoss) {
            self.state.halted = Some(reason);
        }
    }

    /// Lifts a manual or fault halt. Refused after the no-new-entries
    /// cut-off and after the daily loss limit was hit.
    pub fn resume(&mut self, phase: Phase) -> Result<(), &'static str> {
        match self.state.halted {
            None => Ok(()),
            Some(HaltReason::DailyLoss) => Err("the daily loss limit was hit; trading resumes tomorrow"),
            Some(_) if !matches!(phase, Phase::PreOpen | Phase::Open) => {
                Err("too late to resume: past the no-new-entries cut-off")
            }
            Some(_) => {
                self.state.halted = None;
                Ok(())
            }
        }
    }

    pub fn daily_loss_limit(&self) -> Price {
        pct_of(self.state.capital, self.limits.max_daily_loss_pct)
    }

    /// Feeds the day's P&L (realized + unrealized − charges). Returns a
    /// breach the first time a limit is crossed.
    pub fn on_pnl(&mut self, day_pnl: Price) -> Option<Breach> {
        self.state.worst_pnl = self.state.worst_pnl.min(day_pnl);
        if self.state.capital.0 <= 0 {
            return None;
        }
        if day_pnl.0 <= -self.daily_loss_limit().0 && self.state.halted != Some(HaltReason::DailyLoss) {
            self.state.halted = Some(HaltReason::DailyLoss);
            return Some(Breach::FlattenAndHalt);
        }
        if self.limits.daily_profit_lock_pct > 0.0
            && !self.state.profit_locked
            && day_pnl >= pct_of(self.state.capital, self.limits.daily_profit_lock_pct)
        {
            self.state.profit_locked = true;
            return Some(Breach::ProfitLocked);
        }
        None
    }

    /// Records a closed trade's net P&L for the consecutive-loss rule.
    pub fn on_trade_closed(&mut self, now: Ts, net: Price) {
        if net.0 < 0 {
            self.state.consecutive_losses += 1;
            if self.state.consecutive_losses >= self.limits.max_consecutive_losses {
                self.state.cooldown_until = Some(now + TimeDelta::minutes(self.limits.consecutive_loss_cooldown_min));
                self.state.consecutive_losses = 0;
            }
        } else {
            self.state.consecutive_losses = 0;
        }
    }

    /// Whether new entries are possible at all right now (cheap pre-filter
    /// before strategies run): not halted, locked, cooling down or out of
    /// trades.
    pub fn entries_open(&self, now: Ts) -> bool {
        self.state.halted.is_none()
            && !self.state.profit_locked
            && self.state.cooldown_until.is_none_or(|t| now >= t)
            && self.state.trades_today < self.limits.max_trades_per_day
    }

    /// The pre-trade checks, in ARCHITECTURE §5.7 order: kill switch, phase,
    /// feed health, daily P&L, trade count, position count, exposure,
    /// per-trade risk sizing, price band, duplicate. On approval the trade
    /// is counted, so call this only when the order will be sent.
    pub fn check(&mut self, p: &EntryProposal<'_>, book: &Book) -> Result<Approval, Rejection> {
        let a = self.evaluate(p, book)?;
        self.state.trades_today += 1;
        self.recent.insert(key(p), p.now);
        Ok(a)
    }

    fn evaluate(&self, p: &EntryProposal<'_>, book: &Book) -> Result<Approval, Rejection> {
        let l = &self.limits;
        if let Some(h) = &self.state.halted {
            return Err(match h {
                HaltReason::DailyLoss => Rejection::DailyLossHit,
                other => Rejection::Halted(other.clone()),
            });
        }
        if self.state.day != Some(MarketClock::ist_date(p.now)) {
            return Err(Rejection::Phase("day not started".into()));
        }
        if book.phase != Phase::Open {
            return Err(Rejection::Phase(format!("{:?}", book.phase)));
        }
        if !book.feed_ok {
            return Err(Rejection::FeedUnhealthy);
        }
        if self.state.profit_locked {
            return Err(Rejection::ProfitLocked);
        }
        if let Some(until) = self.state.cooldown_until.filter(|&t| p.now < t) {
            return Err(Rejection::Cooldown { until });
        }
        if self.state.trades_today >= l.max_trades_per_day {
            return Err(Rejection::MaxTradesPerDay);
        }
        if book.symbol_busy {
            return Err(Rejection::SymbolBusy);
        }
        if book.open_positions >= l.max_open_positions {
            return Err(Rejection::MaxOpenPositions);
        }
        let capital = self.state.capital;
        if capital.0 <= 0 {
            return Err(Rejection::NoCapital);
        }
        let exposure_cap = Price((capital.0 as f64 * l.max_gross_exposure_x).floor() as i64);
        let room = exposure_cap - book.gross_exposure;
        if room.0 < p.entry.0 {
            return Err(Rejection::Exposure);
        }

        // Sizing.
        let rps = match p.side {
            Side::Buy => p.entry.0 - p.stop.0,
            Side::Sell => p.stop.0 - p.entry.0,
        };
        if p.entry.0 <= 0 || p.stop.0 <= 0 || rps <= 0 {
            return Err(Rejection::BadSignal(format!(
                "stop {} is not on the losing side of entry {}",
                p.stop, p.entry
            )));
        }
        if p.entry < l.min_price() {
            return Err(Rejection::MinPrice);
        }
        let scale = if p.risk_scale.is_finite() {
            p.risk_scale.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let budget = pct_of(capital, l.max_risk_per_trade_pct * scale);
        let qty = (budget.0 / rps).min(room.0 / p.entry.0).min(i64::from(l.max_order_qty));
        if qty <= 0 {
            return Err(Rejection::SizeZero);
        }
        let qty = qty as Qty;

        // Price band and fat-finger checks.
        if let Some(b) = p.band {
            let inside = |x: Price| x > b.lower && x < b.upper;
            if !inside(p.entry) || !inside(p.stop) {
                return Err(Rejection::PriceBand(format!(
                    "entry {} / stop {} outside the circuit band {}–{}",
                    p.entry, p.stop, b.lower, b.upper
                )));
            }
        }
        if let Some(ltp) = p.ltp.filter(|x| x.0 > 0) {
            let dev = (p.entry.0 - ltp.0).abs() as f64 * 100.0 / ltp.0 as f64;
            if dev > l.max_entry_deviation_pct {
                return Err(Rejection::PriceBand(format!(
                    "entry {} is {dev:.2}% from LTP {ltp}",
                    p.entry
                )));
            }
        }
        if let Some(&t) = self.recent.get(&key(p))
            && p.now - t < TimeDelta::seconds(l.duplicate_signal_window_secs)
        {
            return Err(Rejection::Duplicate);
        }
        Ok(Approval {
            qty,
            risk: Price(rps * i64::from(qty)),
        })
    }
}

fn key(p: &EntryProposal<'_>) -> (String, String, bool) {
    (p.token.to_string(), p.strategy.to_string(), p.side == Side::Buy)
}
