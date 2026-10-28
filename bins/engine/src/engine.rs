//! The engine's heart, independent of where ticks and time come from: the
//! daemon feeds it the live feed and the wall clock, `replay` feeds it a
//! recorded session, tests feed it synthetic ticks.
//!
//! Per tick: the paper exchange (if any) plays it, the OMS picks up fills,
//! targets are checked, bars and indicators update, and at each completed
//! indicator bar every strategy on the symbol runs. Entry signals go through
//! the risk guard before the OMS sees them.
//!
//! Per timer (about once a second): broker sync, kill switch, feed health,
//! the daily P&L monitor, reconciliation, and from 15:10 the square-off,
//! repeated until the broker confirms everything is flat.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeDelta};
use saarthi_bars::{BarConfig, BarHub, IndicatorParams, SymbolState};
use saarthi_broker::backtest::CostModel;
use saarthi_broker::{Broker, Paper};
use saarthi_core::{Bar, IST, Instrument, MarketClock, Phase, Price, Tick, Ts};
use saarthi_oms::{Intent, Oms, OmsConfig, OmsEvent, Severity, TradeState};
use saarthi_risk::{Book, Breach, EntryProposal, HaltReason, OrderThrottle, RiskGuard, RiskLimits, RiskState};
use saarthi_store::Db;
use saarthi_strategy::{BarCtx, Direction, Exit, Signal, Strategy, registry};
use serde::{Deserialize, Serialize};

use crate::alerts::SharedAlerts;

const RISK_KEY: &str = "risk";

/// Everything needed to build an [`Engine`].
pub struct Parts<B: Broker + ?Sized> {
    pub broker: Arc<B>,
    /// The paper exchange, when paper trading: ticks are played against it.
    pub paper: Option<Paper>,
    pub db: Db,
    pub limits: RiskLimits,
    pub costs: CostModel,
    pub clock: MarketClock,
    pub instruments: Vec<Instrument>,
    pub strategies: Vec<String>,
    pub direction: Direction,
    pub bars: BarConfig,
    pub indicators: IndicatorParams,
    /// Warm-up history per token on the indicator timeframe, oldest first.
    pub warmup: HashMap<String, Vec<Bar>>,
    pub alerts: SharedAlerts,
    pub halt_file: Option<PathBuf>,
    pub reconcile_every: TimeDelta,
    pub sync_every: TimeDelta,
    pub square_off_retry: TimeDelta,
    pub square_off_alert_after: u32,
}

/// One day's outcome, stored in the `days` table (the M3 exit check reads
/// it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaySummary {
    pub date: NaiveDate,
    pub trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub gross: Price,
    pub charges: Price,
    pub net: Price,
    pub worst_pnl: Price,
    /// Every position flat (confirmed with the broker) after the square-off.
    pub flat_after_square_off: bool,
    pub flat_confirmed_at: Option<Ts>,
    /// Engine starts on this day beyond the first (crashes or restarts).
    pub restarts: u32,
    pub halted: Option<String>,
    pub critical_alerts: u32,
    pub rejected: BTreeMap<String, u32>,
}

#[derive(Debug, Default)]
struct SquareOff {
    started: bool,
    checks: u32,
    last_check: Option<Ts>,
    alerted: bool,
}

pub struct Engine<B: Broker + ?Sized> {
    broker: Arc<B>,
    paper: Option<Paper>,
    clock: MarketClock,
    risk: RiskGuard,
    oms: Oms<B>,
    hub: BarHub,
    tf: saarthi_core::Tf,
    insts: HashMap<String, Instrument>,
    strategies: HashMap<String, Vec<Box<dyn Strategy>>>,
    direction: Direction,
    ltp: HashMap<String, Price>,
    feed_ok: bool,
    feed_critical: bool,
    feed_flattened: bool,
    alerts: SharedAlerts,
    halt_file: Option<PathBuf>,
    reconcile_every: TimeDelta,
    sync_every: TimeDelta,
    square_off_retry: TimeDelta,
    square_off_alert_after: u32,
    last_reconcile: Option<Ts>,
    last_sync: Option<Ts>,
    square: SquareOff,
    summary: DaySummary,
}

impl<B: Broker + ?Sized> Engine<B> {
    /// Builds the engine for the trading day of `now`, restoring today's
    /// risk counters and unfinished trades, and catching up with the broker.
    pub async fn new(p: Parts<B>, now: Ts) -> Result<Self> {
        let day = MarketClock::ist_date(now);
        let saved: Option<RiskState> = p.db.kv_get(RISK_KEY)?;
        let mut risk = match saved {
            Some(s) => RiskGuard::restore(p.limits.clone(), s),
            None => RiskGuard::new(p.limits.clone()),
        };
        let mut summary: DaySummary = p.db.day(day)?.unwrap_or(DaySummary {
            date: day,
            ..DaySummary::default()
        });
        let restarted = risk.state().day == Some(day);
        if restarted {
            summary.restarts += 1;
        }
        let oms = Oms::new(
            p.broker.clone(),
            p.db,
            OrderThrottle::new(p.limits.max_orders_per_second),
            OmsConfig::new(p.costs),
            day,
        )?;
        let mut hub = BarHub::new();
        let mut strategies = HashMap::new();
        let mut insts = HashMap::new();
        for inst in p.instruments {
            let mut st = SymbolState::new(inst.token.clone(), p.bars.clone(), p.indicators.clone());
            if let Some(w) = p.warmup.get(&inst.token) {
                st.warm_up(w);
            }
            hub.insert(st);
            let mut v: Vec<Box<dyn Strategy>> = Vec::new();
            for id in &p.strategies {
                v.push(registry::build(id, &registry::defaults(id)?)?);
            }
            strategies.insert(inst.token.clone(), v);
            insts.insert(inst.token.clone(), inst);
        }
        if !restarted {
            let funds = p.broker.funds().await.context("reading funds for the capital base")?;
            risk.begin_day(day, funds.net);
        }
        let mut e = Engine {
            broker: p.broker,
            paper: p.paper,
            clock: p.clock,
            risk,
            oms,
            hub,
            tf: p.indicators.tf,
            insts,
            strategies,
            direction: p.direction,
            ltp: HashMap::new(),
            feed_ok: true,
            feed_critical: false,
            feed_flattened: false,
            alerts: p.alerts,
            halt_file: p.halt_file,
            reconcile_every: p.reconcile_every,
            sync_every: p.sync_every,
            square_off_retry: p.square_off_retry,
            square_off_alert_after: p.square_off_alert_after,
            last_reconcile: None,
            last_sync: None,
            square: SquareOff::default(),
            summary,
        };
        let recovered = e.oms.recover(now).await?;
        if restarted {
            e.alert(
                Severity::Warning,
                &format!(
                    "engine restarted mid-day ({recovered} unfinished trade(s) recovered, capital base ₹{})",
                    e.risk.capital()
                ),
            );
        }
        e.after(now).await?;
        Ok(e)
    }

    pub fn oms(&self) -> &Oms<B> {
        &self.oms
    }

    pub fn risk(&self) -> &RiskGuard {
        &self.risk
    }

    pub fn summary(&self) -> &DaySummary {
        &self.summary
    }

    pub fn day_pnl(&self) -> Price {
        self.oms.day_pnl(|t| self.ltp.get(t).copied())
    }

    /// Feed health from the feed task (connected and fresh; silent long
    /// enough to flatten).
    pub fn set_feed(&mut self, ok: bool, critical: bool) {
        self.feed_ok = ok;
        self.feed_critical = critical;
        if ok {
            self.feed_flattened = false;
        }
    }

    /// The feed reconnected: bars in flight may be missing trades.
    pub fn mark_gap(&mut self) {
        self.hub.mark_gap();
    }

    fn alert(&mut self, sev: Severity, text: &str) {
        if sev == Severity::Critical {
            self.summary.critical_alerts += 1;
        }
        self.alerts.send(sev, text);
    }

    fn reject(&mut self, code: &str) {
        *self.summary.rejected.entry(code.to_string()).or_default() += 1;
    }

    fn phase(&self, now: Ts) -> Phase {
        self.clock.phase(now).unwrap_or(Phase::Closed)
    }

    /// A new trading day begins (the daemon normally starts fresh each day;
    /// this covers one left running).
    async fn roll_day(&mut self, now: Ts) -> Result<()> {
        let day = MarketClock::ist_date(now);
        if self.oms.day() == day {
            return Ok(());
        }
        self.oms.roll_day(day)?;
        let funds = self.broker.funds().await?;
        self.risk.begin_day(day, funds.net);
        self.summary = DaySummary {
            date: day,
            ..DaySummary::default()
        };
        self.square = SquareOff::default();
        Ok(())
    }

    pub async fn on_tick(&mut self, t: &Tick, now: Ts) -> Result<()> {
        if let Some(paper) = &self.paper {
            let fills = paper.on_tick(t)?;
            if !fills.is_empty() {
                self.oms.sync(now).await?;
            }
        }
        self.ltp.insert(t.token.clone(), t.ltp);
        if self.oms.symbol_busy(&t.token) {
            self.oms.on_ltp(&t.token, t.ltp, now).await?;
        }
        if let Some(u) = self.hub.on_tick(t) {
            for ev in u.bars {
                if ev.tf == self.tf && !ev.partial {
                    self.on_bar(&t.token, ev.bar, now).await?;
                }
            }
        }
        self.after(now).await
    }

    async fn on_bar(&mut self, token: &str, bar: Bar, now: Ts) -> Result<()> {
        let bar_end = bar.ts + TimeDelta::minutes(i64::from(self.tf.minutes()));
        let date = MarketClock::ist_date(bar_end);
        let open_at = MarketClock::at(date, self.clock.times.open);
        let phase = self.phase(now);
        let entries_allowed = phase == Phase::Open && self.risk.entries_open(now) && self.feed_ok;
        let Some(snap) = self.hub.snapshot(token).cloned() else {
            return Ok(());
        };
        let Some(mut strats) = self.strategies.remove(token) else {
            return Ok(());
        };
        let mut wanted: Vec<(String, Signal)> = Vec::new();
        let mut exits: Vec<(String, String)> = Vec::new();
        for s in strats.iter_mut() {
            let live = self.oms.trade_for(token, s.id());
            let ctx = BarCtx {
                bar,
                now: bar_end,
                since_open: bar_end - open_at,
                entries_allowed,
                position: live.filter(|t| t.state != TradeState::Pending).map(|t| t.side),
                direction: self.direction,
            };
            let sig = s.on_bar(&snap, &ctx);
            match live {
                Some(t) if t.state == TradeState::Open => {
                    if let Exit::Close(why) = s.manage(&snap, &ctx) {
                        exits.push((t.id.clone(), why));
                    }
                }
                Some(_) => {}
                None => {
                    if let Some(sig) = sig.filter(|g| entries_allowed && self.direction.allows(g.side)) {
                        wanted.push((s.id().to_string(), sig));
                    }
                }
            }
        }
        self.strategies.insert(token.to_string(), strats);
        for (id, why) in exits {
            self.oms.request_exit(&id, &why, now).await?;
        }
        for (strategy, sig) in wanted {
            self.try_enter(&strategy, sig, now).await?;
        }
        Ok(())
    }

    async fn try_enter(&mut self, strategy: &str, sig: Signal, now: Ts) -> Result<()> {
        let Some(inst) = self.insts.get(&sig.token).cloned() else {
            return Ok(());
        };
        let ltp = |t: &str| self.ltp.get(t).copied();
        let book = Book {
            phase: self.phase(now),
            feed_ok: self.feed_ok,
            open_positions: self.oms.open_count(),
            gross_exposure: self.oms.gross_exposure(ltp),
            symbol_busy: self.oms.symbol_busy(&sig.token),
        };
        let proposal = EntryProposal {
            now,
            token: &sig.token,
            strategy,
            side: sig.side,
            entry: sig.entry,
            stop: sig.stop,
            ltp: self.ltp.get(&sig.token).copied(),
            band: None,
            risk_scale: 1.0,
        };
        match self.risk.check(&proposal, &book) {
            Ok(a) => {
                let intent = Intent {
                    strategy: strategy.to_string(),
                    token: inst.token.clone(),
                    symbol: inst.symbol.clone(),
                    exchange: inst.exchange,
                    side: sig.side,
                    qty: a.qty,
                    signal_px: sig.entry,
                    stop: sig.stop,
                    target: sig.target,
                    planned_risk: a.risk,
                    reason: sig.reason,
                };
                self.save_risk()?;
                if let Err(e) = self.oms.open(intent, now).await {
                    self.alert(Severity::Warning, &format!("entry for {} failed: {e}", inst.symbol));
                }
            }
            Err(r) => {
                tracing::debug!(symbol = %inst.symbol, strategy, reason = %r, "entry refused");
                self.reject(r.code());
            }
        }
        Ok(())
    }

    /// The periodic checks. Call about once a second.
    pub async fn on_timer(&mut self, now: Ts) -> Result<()> {
        let trading_day = self.clock.is_trading_day(MarketClock::ist_date(now)).unwrap_or(false);
        if trading_day {
            self.roll_day(now).await?;
        }
        let phase = self.phase(now);
        for (tok, ev) in self.hub.on_time(now) {
            if ev.tf == self.tf && !ev.partial {
                self.on_bar(&tok, ev.bar, now).await?;
            }
        }
        if self.paper.is_some() || self.last_sync.is_none_or(|t| now - t >= self.sync_every) {
            self.oms.sync(now).await?;
            self.last_sync = Some(now);
        }
        self.kill_switch(now, phase).await?;
        if self.feed_critical && !self.feed_flattened && self.oms.live_trades().next().is_some() {
            self.feed_flattened = true;
            self.alert(
                Severity::Critical,
                "feed silent too long with open positions: flattening",
            );
            self.oms.flatten_all("feed_stale", now).await?;
        }
        if self.last_reconcile.is_none_or(|t| now - t >= self.reconcile_every) {
            self.last_reconcile = Some(now);
            let rec = self.oms.reconcile(now).await?;
            if rec.severe {
                self.risk.halt(HaltReason::Fault(format!(
                    "reconciliation: {}",
                    rec.mismatches.join("; ")
                )));
                self.oms.flatten_all("reconcile_halt", now).await?;
            }
        }
        if matches!(phase, Phase::SquareOff) || (trading_day && phase == Phase::Closed && self.past_square_off(now)) {
            self.square_off(now).await?;
        }
        self.after(now).await
    }

    fn past_square_off(&self, now: Ts) -> bool {
        now.with_timezone(&IST).time() >= self.clock.times.square_off
    }

    async fn kill_switch(&mut self, now: Ts, phase: Phase) -> Result<()> {
        let Some(path) = self.halt_file.clone() else {
            return Ok(());
        };
        let present = path.exists();
        let manual = matches!(self.risk.halted(), Some(HaltReason::Manual(_)));
        if present && !manual {
            let why = std::fs::read_to_string(&path).unwrap_or_default();
            let why = why.trim();
            self.risk.halt(HaltReason::Manual(if why.is_empty() {
                "HALT file".into()
            } else {
                why.to_string()
            }));
            self.alert(Severity::Critical, "kill switch: halted, flattening everything");
            self.oms.flatten_all("halt", now).await?;
        } else if !present && manual {
            match self.risk.resume(phase) {
                Ok(()) => self.alert(Severity::Warning, "kill switch removed: trading resumed"),
                Err(why) => tracing::debug!(why, "kill switch removed but resume refused"),
            }
        }
        Ok(())
    }

    async fn square_off(&mut self, now: Ts) -> Result<()> {
        if !self.square.started {
            self.square.started = true;
            self.alert(Severity::Info, "square-off: closing every position");
            self.oms.flatten_all("square_off", now).await?;
        }
        if self.summary.flat_after_square_off {
            return Ok(());
        }
        if self.square.last_check.is_some_and(|t| now - t < self.square_off_retry) {
            return Ok(());
        }
        self.square.last_check = Some(now);
        self.square.checks += 1;
        self.oms.sync(now).await?;
        let open = self.oms.broker_open_positions().await?;
        if open.is_empty() && self.oms.live_trades().next().is_none() {
            self.summary.flat_after_square_off = true;
            self.summary.flat_confirmed_at = Some(now);
            self.alert(Severity::Info, "square-off complete: flat");
            return Ok(());
        }
        // Anything the broker still holds that we no longer track is exited
        // by reconciliation; live trades keep retrying their exits.
        self.oms.flatten_all("square_off", now).await?;
        if self.square.checks >= self.square_off_alert_after && !self.square.alerted {
            self.square.alerted = true;
            let what: Vec<String> = open.iter().map(|p| format!("{} {}", p.symbol, p.net_qty)).collect();
            self.alert(
                Severity::Critical,
                &format!("NOT FLAT after square-off, still retrying: {}", what.join(", ")),
            );
        }
        Ok(())
    }

    /// Bookkeeping after every step: OMS events into risk and alerts, the
    /// P&L monitor, persistence.
    async fn after(&mut self, now: Ts) -> Result<()> {
        for (ts, ev) in self.oms.take_events() {
            match ev {
                OmsEvent::Closed {
                    net, symbol, reason, ..
                } => {
                    self.risk.on_trade_closed(ts, net);
                    tracing::info!(%symbol, %net, reason, "trade closed");
                }
                OmsEvent::EntryRejected { reason, trade_id } => {
                    self.reject("broker_rejected");
                    tracing::warn!(trade_id, reason, "entry rejected by broker");
                }
                OmsEvent::Alert { severity, message } if severity != Severity::Info => {
                    self.alert(severity, &message);
                }
                _ => {}
            }
        }
        let pnl = self.day_pnl();
        match self.risk.on_pnl(pnl) {
            Some(Breach::FlattenAndHalt) => {
                self.alert(
                    Severity::Critical,
                    &format!(
                        "daily loss limit hit (₹{pnl} vs limit ₹{}): flattening and halting for the day",
                        self.risk.daily_loss_limit()
                    ),
                );
                self.oms.flatten_all("daily_loss", now).await?;
            }
            Some(Breach::ProfitLocked) => {
                self.alert(
                    Severity::Info,
                    &format!("profit lock reached (₹{pnl}): no new entries today"),
                );
            }
            None => {}
        }
        self.refresh_summary();
        self.save_risk()?;
        Ok(())
    }

    fn refresh_summary(&mut self) {
        let day = self.oms.day();
        let mut s = DaySummary {
            date: day,
            flat_after_square_off: self.summary.flat_after_square_off,
            flat_confirmed_at: self.summary.flat_confirmed_at,
            restarts: self.summary.restarts,
            critical_alerts: self.summary.critical_alerts,
            rejected: std::mem::take(&mut self.summary.rejected),
            halted: self.risk.halted().map(|h| h.to_string()),
            worst_pnl: self.risk.state().worst_pnl,
            ..DaySummary::default()
        };
        for t in self.oms.trades().filter(|t| t.day == day && t.entry_qty > 0) {
            s.trades += 1;
            let net = t.net(self.ltp.get(&t.token).copied());
            if t.state == TradeState::Closed {
                if net.0 > 0 {
                    s.wins += 1;
                } else {
                    s.losses += 1;
                }
            }
            s.gross = s.gross + t.realized() + t.unrealized(self.ltp.get(&t.token).copied());
            s.charges = s.charges + t.charges;
            s.net = s.net + net;
        }
        self.summary = s;
    }

    fn save_risk(&mut self) -> Result<()> {
        let state = self.risk.state();
        let summary = self.summary.clone();
        self.oms.db_mut().write(|w| {
            w.kv_put(RISK_KEY, &state)?;
            w.put_day(summary.date, &summary)
        })?;
        Ok(())
    }
}
