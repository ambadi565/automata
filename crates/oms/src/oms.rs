//! The order manager.
//!
//! Driven by [`Oms::sync`]: read the broker's order book, apply status
//! changes and fills to the local orders, then advance every live trade's
//! state machine (place or resize its stop, cancel and exit, close). The same
//! loop runs after a restart, which is what makes recovery work: local state
//! comes back from SQLite, the broker's book says what happened meanwhile,
//! and missing stops are re-armed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use chrono::{NaiveDate, TimeDelta};
use saarthi_broker::backtest::CostModel;
use saarthi_broker::{
    Broker, BrokerError, BrokerOrder, Modify, OrderKind, OrderRequest, OrderStatus, Position, Product,
};
use saarthi_core::{MarketClock, Price, Qty, Side, Symbol, Ts};
use saarthi_risk::OrderThrottle;
use saarthi_store::{Db, FillRow};
use serde::Serialize;

use crate::error::{OmsError, OmsResult};
use crate::model::{Intent, OrderRec, Role, Trade, TradeState};

#[derive(Debug, Clone)]
pub struct OmsConfig {
    /// Estimates charges per fill (the broker does not report them live).
    pub costs: CostModel,
    pub product: Product,
    /// An order the broker has never heard of after this long is lost.
    pub lost_order_grace: TimeDelta,
    /// Alert (critical) after this many failed exit attempts on one trade.
    pub max_exit_failures: u32,
    /// Prefix of every order tag (audit / compliance).
    pub tag: String,
}

impl OmsConfig {
    pub fn new(costs: CostModel) -> Self {
        OmsConfig {
            costs,
            product: Product::Intraday,
            lost_order_grace: TimeDelta::seconds(15),
            max_exit_failures: 5,
            tag: "saarthi".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// What happened, for the engine (risk updates, alerts) and the event log.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OmsEvent {
    EntrySent {
        trade_id: String,
        symbol: Symbol,
        side: Side,
        qty: Qty,
    },
    EntryRejected {
        trade_id: String,
        reason: String,
    },
    Opened {
        trade_id: String,
        qty: Qty,
        avg: Price,
    },
    StopPlaced {
        trade_id: String,
        trigger: Price,
        qty: Qty,
    },
    ExitRequested {
        trade_id: String,
        reason: String,
    },
    Closed {
        trade_id: String,
        symbol: Symbol,
        net: Price,
        reason: String,
    },
    Alert {
        severity: Severity,
        message: String,
    },
}

/// Result of comparing local positions with the broker's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reconciliation {
    /// Human-readable differences found (confirmed on two checks in a row).
    pub mismatches: Vec<String>,
    /// The broker holds positions the OMS did not know about (or more of
    /// them): trading on this state is unsafe.
    pub severe: bool,
}

pub struct Oms<B: Broker + ?Sized> {
    broker: Arc<B>,
    db: Db,
    throttle: OrderThrottle,
    cfg: OmsConfig,
    day: NaiveDate,
    seq: u32,
    trades: BTreeMap<String, Trade>,
    orders: HashMap<String, OrderRec>,
    dirty_trades: HashSet<String>,
    dirty_orders: HashSet<String>,
    /// For the engine; drained by [`Oms::take_events`].
    events: Vec<(Ts, OmsEvent)>,
    /// For the event log; drained by `flush`.
    unlogged: Vec<(Ts, OmsEvent)>,
    /// Reconciliation differences seen once, awaiting confirmation.
    suspect: HashMap<Symbol, (i64, i64)>,
    /// Something was sent to the broker during this sync pass.
    acted: bool,
}

impl<B: Broker + ?Sized> Oms<B> {
    /// Loads today's trades and every unfinished one from the store. After a
    /// crash, call [`Oms::recover`] next.
    pub fn new(broker: Arc<B>, db: Db, throttle: OrderThrottle, cfg: OmsConfig, day: NaiveDate) -> OmsResult<Self> {
        let mut trades = BTreeMap::new();
        for t in db.trades_on::<Trade>(day)? {
            trades.insert(t.id.clone(), t);
        }
        for t in db.trades_excluding::<Trade>(&TradeState::DONE)? {
            trades.insert(t.id.clone(), t);
        }
        let mut orders = HashMap::new();
        for t in trades.values().filter(|t| !t.state.is_done()) {
            for o in db.orders_of::<OrderRec>(&t.id)? {
                orders.insert(o.client_id.clone(), o);
            }
        }
        let seq = db.trade_count_on(day)?;
        Ok(Oms {
            broker,
            db,
            throttle,
            cfg,
            day,
            seq,
            trades,
            orders,
            dirty_trades: HashSet::new(),
            dirty_orders: HashSet::new(),
            events: Vec::new(),
            unlogged: Vec::new(),
            suspect: HashMap::new(),
            acted: false,
        })
    }

    /// After a restart: catch up with the broker (fills and cancels that
    /// happened while down, orders sent but never recorded), re-arm missing
    /// stops, and report what was found.
    pub async fn recover(&mut self, now: Ts) -> OmsResult<usize> {
        let live = self.live_trades().count();
        if live > 0 {
            self.alert(
                now,
                Severity::Warning,
                format!("recovering {live} unfinished trade(s) from the store"),
            );
        }
        self.sync(now).await?;
        Ok(live)
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn db_mut(&mut self) -> &mut Db {
        &mut self.db
    }

    pub fn day(&self) -> NaiveDate {
        self.day
    }

    /// A new trading day: forget finished trades, restart numbering.
    pub fn roll_day(&mut self, day: NaiveDate) -> OmsResult<()> {
        if day == self.day {
            return Ok(());
        }
        self.day = day;
        self.seq = self.db.trade_count_on(day)?;
        self.trades.retain(|_, t| !t.state.is_done());
        Ok(())
    }

    pub fn trades(&self) -> impl Iterator<Item = &Trade> {
        self.trades.values()
    }

    pub fn trade(&self, id: &str) -> Option<&Trade> {
        self.trades.get(id)
    }

    pub fn order(&self, client_id: &str) -> Option<&OrderRec> {
        self.orders.get(client_id)
    }

    pub fn live_trades(&self) -> impl Iterator<Item = &Trade> {
        self.trades.values().filter(|t| !t.state.is_done())
    }

    /// Positions open or entries working (counts symbols once).
    pub fn open_count(&self) -> usize {
        self.live_trades().map(|t| &t.token).collect::<HashSet<_>>().len()
    }

    pub fn symbol_busy(&self, token: &str) -> bool {
        self.live_trades().any(|t| t.token == token)
    }

    /// The live trade on `token` from `strategy`, if any.
    pub fn trade_for(&self, token: &str, strategy: &str) -> Option<&Trade> {
        self.live_trades().find(|t| t.token == token && t.strategy == strategy)
    }

    pub fn gross_exposure(&self, ltp: impl Fn(&str) -> Option<Price>) -> Price {
        self.live_trades().map(|t| t.exposure(ltp(&t.token))).sum()
    }

    /// Today's P&L: realized + unrealized − estimated charges.
    pub fn day_pnl(&self, ltp: impl Fn(&str) -> Option<Price>) -> Price {
        self.trades
            .values()
            .filter(|t| t.day == self.day || !t.state.is_done())
            .map(|t| t.net(ltp(&t.token)))
            .sum()
    }

    pub fn take_events(&mut self) -> Vec<(Ts, OmsEvent)> {
        std::mem::take(&mut self.events)
    }

    fn emit(&mut self, now: Ts, ev: OmsEvent) {
        match &ev {
            OmsEvent::Alert { severity, message } => match severity {
                Severity::Critical => tracing::error!(%message, "oms"),
                Severity::Warning => tracing::warn!(%message, "oms"),
                Severity::Info => tracing::info!(%message, "oms"),
            },
            other => tracing::info!(event = ?other, "oms"),
        }
        self.unlogged.push((now, ev.clone()));
        self.events.push((now, ev));
    }

    fn alert(&mut self, now: Ts, severity: Severity, message: String) {
        self.emit(now, OmsEvent::Alert { severity, message });
    }

    fn touch_trade(&mut self, id: &str) {
        self.dirty_trades.insert(id.to_string());
    }

    fn touch_order(&mut self, cid: &str) {
        self.dirty_orders.insert(cid.to_string());
    }

    /// Writes every changed trade and order plus new events in one
    /// transaction.
    fn flush(&mut self) -> OmsResult<()> {
        if self.dirty_trades.is_empty() && self.dirty_orders.is_empty() && self.unlogged.is_empty() {
            return Ok(());
        }
        let trades: Vec<&Trade> = self.dirty_trades.iter().filter_map(|id| self.trades.get(id)).collect();
        let orders: Vec<&OrderRec> = self.dirty_orders.iter().filter_map(|c| self.orders.get(c)).collect();
        let events = &self.unlogged;
        self.db.write(|w| {
            for t in &trades {
                w.put_trade(&t.id, t.day, &t.token, t.state.as_str(), t)?;
            }
            for o in &orders {
                w.put_order(
                    &o.client_id,
                    &o.trade_id,
                    o.broker_id.as_ref().map(|b| b.0.as_str()),
                    o.status.as_str(),
                    o,
                )?;
            }
            for (ts, ev) in events {
                w.event(&ts.to_rfc3339(), event_kind(ev), ev)?;
            }
            Ok(())
        })?;
        self.unlogged.clear();
        self.dirty_trades.clear();
        self.dirty_orders.clear();
        Ok(())
    }
}

fn event_kind(ev: &OmsEvent) -> &'static str {
    match ev {
        OmsEvent::EntrySent { .. } => "entry_sent",
        OmsEvent::EntryRejected { .. } => "entry_rejected",
        OmsEvent::Opened { .. } => "opened",
        OmsEvent::StopPlaced { .. } => "stop_placed",
        OmsEvent::ExitRequested { .. } => "exit_requested",
        OmsEvent::Closed { .. } => "closed",
        OmsEvent::Alert { .. } => "alert",
    }
}

/// Price of the newest `delta` shares, given the order's average before and
/// after them.
fn delta_price(prev_avg: Option<Price>, prev_qty: Qty, new_avg: Price, new_qty: Qty) -> Price {
    let delta = i64::from(new_qty - prev_qty);
    let before = prev_avg.map_or(0, |p| p.0 * i64::from(prev_qty));
    let after = new_avg.0 * i64::from(new_qty);
    // Round half away from zero.
    let num = after - before;
    Price((num + num.signum() * delta / 2) / delta)
}

impl<B: Broker + ?Sized> Oms<B> {
    fn next_trade_id(&mut self) -> String {
        self.seq += 1;
        format!("{}-{:04}", self.day.format("%y%m%d"), self.seq)
    }

    fn new_order(&mut self, trade_id: &str, role: Role, side: Side, qty: Qty, kind: OrderKind, now: Ts) -> String {
        let t = &self.trades[trade_id];
        let n = t
            .orders
            .iter()
            .filter(|c| self.orders.get(*c).is_some_and(|o| o.role == role))
            .count();
        let suffix = match role {
            Role::Entry => "E".to_string(),
            Role::Stop => format!("S{}", n + 1),
            Role::Exit => format!("X{}", n + 1),
        };
        let client_id = format!("{trade_id}-{suffix}");
        let req = OrderRequest {
            client_order_id: client_id.clone(),
            symbol: t.symbol.clone(),
            exchange: t.exchange,
            token: t.token.clone(),
            side,
            qty,
            kind,
            product: self.cfg.product,
            tag: format!("{}-{}", self.cfg.tag, t.strategy),
        };
        let rec = OrderRec {
            client_id: client_id.clone(),
            broker_id: None,
            trade_id: trade_id.to_string(),
            role,
            req,
            status: OrderStatus::New,
            filled: 0,
            avg_price: None,
            reject_reason: None,
            created: now,
            updated: now,
        };
        self.orders.insert(client_id.clone(), rec);
        self.trades
            .get_mut(trade_id)
            .expect("trade")
            .orders
            .push(client_id.clone());
        self.touch_order(&client_id);
        self.touch_trade(trade_id);
        client_id
    }

    /// Persists the order as `New`, then sends it. A broker rejection marks
    /// it rejected; a transport error leaves it `Sent` for the next sync to
    /// find (or declare lost).
    async fn send(&mut self, cid: &str, now: Ts) -> OmsResult<()> {
        self.flush()?;
        let req = self.orders[cid].req.clone();
        self.acted = true;
        self.throttle.acquire().await;
        let res = self.broker.place(req).await;
        let o = self.orders.get_mut(cid).expect("order");
        o.updated = now;
        match res {
            Ok(id) => {
                o.broker_id = Some(id);
                o.status = OrderStatus::Sent;
            }
            Err(e @ (BrokerError::Api { .. } | BrokerError::Unsupported(_) | BrokerError::Config(_))) => {
                o.status = OrderStatus::Rejected;
                o.reject_reason = Some(e.to_string());
            }
            Err(e) => {
                o.status = OrderStatus::Sent;
                tracing::warn!(client_id = cid, error = %e, "order outcome unknown; will look for it in the order book");
            }
        }
        self.touch_order(cid);
        self.flush()
    }

    async fn cancel_order(&mut self, cid: &str, now: Ts) -> OmsResult<()> {
        let Some(id) = self.orders[cid].broker_id.clone() else {
            return Ok(());
        };
        self.acted = true;
        self.throttle.acquire().await;
        if let Err(e) = self.broker.cancel(&id).await {
            // Usually it filled or was cancelled meanwhile; the next sync
            // tells which.
            tracing::debug!(client_id = cid, error = %e, "cancel failed");
        }
        self.orders.get_mut(cid).expect("order").updated = now;
        Ok(())
    }

    /// Opens a trade: records it and its entry (market) order, then sends
    /// the entry. Returns the trade id. The stop goes in when the entry
    /// fills.
    pub async fn open(&mut self, i: Intent, now: Ts) -> OmsResult<String> {
        if i.qty == 0 {
            return Err(OmsError::Invalid("quantity must be positive".into()));
        }
        let ok = match i.side {
            Side::Buy => i.stop < i.signal_px,
            Side::Sell => i.stop > i.signal_px,
        };
        if !ok || i.stop.0 <= 0 {
            return Err(OmsError::Invalid(format!(
                "stop {} is on the wrong side of {}",
                i.stop, i.signal_px
            )));
        }
        if self.symbol_busy(&i.token) {
            return Err(OmsError::Invalid(format!("{} already has a live trade", i.symbol)));
        }
        let id = self.next_trade_id();
        let t = Trade {
            id: id.clone(),
            day: MarketClock::ist_date(now),
            strategy: i.strategy,
            token: i.token,
            symbol: i.symbol.clone(),
            exchange: i.exchange,
            side: i.side,
            planned_qty: i.qty,
            signal_px: i.signal_px,
            signal_ts: now,
            stop: i.stop,
            target: i.target,
            planned_risk: i.planned_risk,
            reason: i.reason,
            state: TradeState::Pending,
            entry_qty: 0,
            entry_value: 0,
            exit_qty: 0,
            exit_value: 0,
            charges: Price::ZERO,
            exit_reason: None,
            opened_at: None,
            closed_at: None,
            orders: Vec::new(),
            exit_failures: 0,
        };
        self.trades.insert(id.clone(), t);
        self.touch_trade(&id);
        let cid = self.new_order(&id, Role::Entry, i.side, i.qty, OrderKind::Market, now);
        self.emit(
            now,
            OmsEvent::EntrySent {
                trade_id: id.clone(),
                symbol: i.symbol,
                side: i.side,
                qty: i.qty,
            },
        );
        self.send(&cid, now).await?;
        self.advance(&id, now).await?;
        self.flush()?;
        Ok(id)
    }

    /// Reads the broker's order book, applies it, and advances every live
    /// trade.
    ///
    /// When the pass sent anything (a stop, a cancel, an exit), the book is
    /// read once more so fast answers (an immediate reject, a confirmed
    /// cancel) are acted on now rather than at the next sync.
    pub async fn sync(&mut self, now: Ts) -> OmsResult<()> {
        for _ in 0..3 {
            self.acted = false;
            let book = self.broker.order_book().await?;
            self.apply_book(&book, now);
            let live: Vec<String> = self.live_trades().map(|t| t.id.clone()).collect();
            for id in live {
                self.advance(&id, now).await?;
            }
            self.flush()?;
            if !self.acted {
                break;
            }
        }
        Ok(())
    }

    fn apply_book(&mut self, book: &[BrokerOrder], now: Ts) {
        let by_id: HashMap<&str, &BrokerOrder> = book.iter().map(|b| (b.id.0.as_str(), b)).collect();
        let by_client: HashMap<&str, &BrokerOrder> = book
            .iter()
            .filter_map(|b| b.client_order_id.as_deref().map(|c| (c, b)))
            .collect();
        let working: Vec<String> = self
            .orders
            .values()
            .filter(|o| o.is_working())
            .map(|o| o.client_id.clone())
            .collect();
        for cid in working {
            let o = &self.orders[&cid];
            let found = match &o.broker_id {
                Some(id) => by_id.get(id.0.as_str()).copied(),
                None => by_client.get(cid.as_str()).copied(),
            };
            match found {
                Some(bo) => {
                    let bo = bo.clone();
                    self.apply_order(&cid, &bo, now);
                }
                None if now - o.created > self.cfg.lost_order_grace => {
                    let msg = match o.broker_id {
                        Some(ref id) => format!("order {cid} ({}) vanished from the broker's book", id.0),
                        None => format!("order {cid} never reached the broker"),
                    };
                    let o = self.orders.get_mut(&cid).expect("order");
                    o.status = OrderStatus::Cancelled;
                    o.reject_reason = Some("lost".into());
                    o.updated = now;
                    self.touch_order(&cid);
                    self.alert(now, Severity::Warning, msg);
                }
                None => {}
            }
        }
    }

    fn apply_order(&mut self, cid: &str, bo: &BrokerOrder, now: Ts) {
        let (role, trade_id, side, prev_qty, prev_avg) = {
            let o = &self.orders[cid];
            (o.role, o.trade_id.clone(), o.req.side, o.filled, o.avg_price)
        };
        let mut changed = false;
        if bo.filled_qty < prev_qty {
            self.alert(
                now,
                Severity::Critical,
                format!("broker shows {} filled on {cid}, we recorded {prev_qty}", bo.filled_qty),
            );
        } else if bo.filled_qty > prev_qty {
            let delta = bo.filled_qty - prev_qty;
            let px = match bo.avg_price {
                Some(avg) => delta_price(prev_avg, prev_qty, avg, bo.filled_qty),
                None => {
                    let t = &self.trades[&trade_id];
                    t.avg_entry().unwrap_or(t.signal_px)
                }
            };
            let charges = self.cfg.costs.charges(side, px, delta).total();
            let fill = FillRow {
                client_id: cid.to_string(),
                cum_qty: bo.filled_qty,
                qty: delta,
                price: px.0,
                ts: now.to_rfc3339(),
            };
            if let Err(e) = self.db.write(|w| w.add_fill(&fill)) {
                tracing::error!(error = %e, "recording fill");
            }
            let t = self.trades.get_mut(&trade_id).expect("trade");
            match role {
                Role::Entry => {
                    t.entry_qty += delta;
                    t.entry_value += px.0 * i64::from(delta);
                }
                Role::Stop | Role::Exit => {
                    let q = delta.min(t.open_qty());
                    t.exit_qty += q;
                    t.exit_value += px.0 * i64::from(q);
                    if role == Role::Stop && t.exit_reason.is_none() {
                        t.exit_reason = Some("stop".into());
                    }
                }
            }
            t.charges = t.charges + charges;
            self.touch_trade(&trade_id);
            let o = self.orders.get_mut(cid).expect("order");
            o.filled = bo.filled_qty;
            o.avg_price = bo.avg_price.or(Some(px));
            changed = true;
        }
        let o = self.orders.get_mut(cid).expect("order");
        if o.broker_id.is_none() {
            o.broker_id = Some(bo.id.clone());
            changed = true;
        }
        if o.status != bo.status && !(o.status == OrderStatus::Sent && bo.status == OrderStatus::New) {
            o.status = bo.status;
            o.reject_reason = bo.reject_reason.clone().or(o.reject_reason.take());
            changed = true;
        }
        if changed {
            o.updated = now;
            self.touch_order(cid);
        }
    }

    fn orders_of(&self, t: &Trade, role: Role) -> Vec<OrderRec> {
        t.orders
            .iter()
            .filter_map(|c| self.orders.get(c))
            .filter(|o| o.role == role)
            .cloned()
            .collect()
    }

    /// Runs one trade's state machine as far as it can go without waiting
    /// for the broker.
    async fn advance(&mut self, id: &str, now: Ts) -> OmsResult<()> {
        for _ in 0..4 {
            let t = self.trades[id].clone();
            let entries = self.orders_of(&t, Role::Entry);
            let stops = self.orders_of(&t, Role::Stop);
            let exits = self.orders_of(&t, Role::Exit);
            let entry_working = entries.iter().any(OrderRec::is_working);
            let working_stops: Vec<&OrderRec> = stops.iter().filter(|o| o.is_working()).collect();
            let exit_working = exits.iter().any(OrderRec::is_working);
            match t.state {
                TradeState::Closed | TradeState::Cancelled => return Ok(()),
                TradeState::Pending if t.entry_qty > 0 => {
                    let tr = self.trades.get_mut(id).expect("trade");
                    tr.state = TradeState::Open;
                    tr.opened_at = Some(now);
                    let ev = OmsEvent::Opened {
                        trade_id: id.to_string(),
                        qty: tr.entry_qty,
                        avg: tr.avg_entry().unwrap_or_default(),
                    };
                    self.touch_trade(id);
                    self.emit(now, ev);
                    continue;
                }
                TradeState::Pending => {
                    if !entry_working {
                        let reason = entries
                            .last()
                            .and_then(|e| e.reject_reason.clone())
                            .unwrap_or_else(|| "entry cancelled".into());
                        self.finish(id, TradeState::Cancelled, now);
                        self.emit(
                            now,
                            OmsEvent::EntryRejected {
                                trade_id: id.to_string(),
                                reason,
                            },
                        );
                    }
                    return Ok(());
                }
                TradeState::Open => {
                    let open = t.open_qty();
                    if open == 0 {
                        if entry_working {
                            // Stopped out of a partial fill while the rest of
                            // the entry still works: cancel it.
                            for e in entries.iter().filter(|e| e.is_working()) {
                                self.cancel_order(&e.client_id, now).await?;
                            }
                            return Ok(());
                        }
                        if working_stops.is_empty() {
                            self.finish(id, TradeState::Closed, now);
                            return Ok(());
                        }
                        // Fully exited with a stop still working: cancel it.
                        for s in working_stops {
                            self.cancel_order(&s.client_id, now).await?;
                        }
                        return Ok(());
                    }
                    match working_stops.as_slice() {
                        [] => {
                            if let Some(last) = stops.last().filter(|s| s.status == OrderStatus::Rejected) {
                                let why = last.reject_reason.clone().unwrap_or_default();
                                self.alert(
                                    now,
                                    Severity::Critical,
                                    format!("stop for {id} rejected ({why}); exiting at market"),
                                );
                                self.begin_exit(id, "stop_rejected", now);
                                continue;
                            }
                            let cid = self.new_order(
                                id,
                                Role::Stop,
                                t.side.opposite(),
                                open,
                                OrderKind::StopLossMarket { trigger: t.stop },
                                now,
                            );
                            self.send(&cid, now).await?;
                            if !stops.is_empty() {
                                self.alert(now, Severity::Warning, format!("re-armed missing stop for {id}"));
                            }
                            self.emit(
                                now,
                                OmsEvent::StopPlaced {
                                    trade_id: id.to_string(),
                                    trigger: t.stop,
                                    qty: open,
                                },
                            );
                            continue;
                        }
                        [s] => {
                            let want = s.filled + open;
                            if s.req.qty != want && s.broker_id.is_some() {
                                let bid = s.broker_id.clone().expect("checked");
                                self.acted = true;
                                self.throttle.acquire().await;
                                match self
                                    .broker
                                    .modify(
                                        &bid,
                                        Modify {
                                            qty: Some(want),
                                            ..Modify::default()
                                        },
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        let o = self.orders.get_mut(&s.client_id).expect("order");
                                        o.req.qty = want;
                                        o.updated = now;
                                        self.touch_order(&s.client_id.clone());
                                    }
                                    Err(e) => tracing::warn!(trade = id, error = %e, "resizing stop failed"),
                                }
                            }
                            return Ok(());
                        }
                        many => {
                            // Never more than one protective stop: keep the
                            // first.
                            for s in &many[1..] {
                                self.cancel_order(&s.client_id, now).await?;
                            }
                            return Ok(());
                        }
                    }
                }
                TradeState::Exiting => {
                    if entry_working {
                        for e in entries.iter().filter(|e| e.is_working()) {
                            self.cancel_order(&e.client_id, now).await?;
                        }
                        return Ok(());
                    }
                    if !working_stops.is_empty() {
                        for s in working_stops {
                            self.cancel_order(&s.client_id, now).await?;
                        }
                        return Ok(());
                    }
                    let open = t.open_qty();
                    if open == 0 {
                        let state = if t.entry_qty == 0 {
                            TradeState::Cancelled
                        } else {
                            TradeState::Closed
                        };
                        self.finish(id, state, now);
                        return Ok(());
                    }
                    if exit_working {
                        return Ok(());
                    }
                    if let Some(last) = exits.last().filter(|x| x.filled < x.req.qty) {
                        let why = last
                            .reject_reason
                            .clone()
                            .unwrap_or_else(|| last.status.as_str().into());
                        let tr = self.trades.get_mut(id).expect("trade");
                        tr.exit_failures += 1;
                        let n = tr.exit_failures;
                        self.touch_trade(id);
                        let sev = if n >= self.cfg.max_exit_failures {
                            Severity::Critical
                        } else {
                            Severity::Warning
                        };
                        self.alert(now, sev, format!("exit {n} for {id} failed ({why}); retrying"));
                    }
                    let cid = self.new_order(id, Role::Exit, t.side.opposite(), open, OrderKind::Market, now);
                    self.send(&cid, now).await?;
                    continue;
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, id: &str, state: TradeState, now: Ts) {
        let t = self.trades.get_mut(id).expect("trade");
        t.state = state;
        t.closed_at = Some(now);
        let ev = (state == TradeState::Closed).then(|| OmsEvent::Closed {
            trade_id: id.to_string(),
            symbol: t.symbol.clone(),
            net: t.net(None),
            reason: t.exit_reason.clone().unwrap_or_else(|| "exit".into()),
        });
        self.touch_trade(id);
        if let Some(ev) = ev {
            self.emit(now, ev);
        }
    }

    fn begin_exit(&mut self, id: &str, reason: &str, now: Ts) -> bool {
        let t = self.trades.get_mut(id).expect("trade");
        if t.state.is_done() || t.state == TradeState::Exiting {
            return false;
        }
        t.state = TradeState::Exiting;
        if t.exit_reason.is_none() {
            t.exit_reason = Some(reason.to_string());
        }
        self.touch_trade(id);
        self.emit(
            now,
            OmsEvent::ExitRequested {
                trade_id: id.to_string(),
                reason: reason.to_string(),
            },
        );
        true
    }

    /// Closes a trade at market: cancel its entry and stop, then exit what
    /// is open, retrying until flat.
    pub async fn request_exit(&mut self, id: &str, reason: &str, now: Ts) -> OmsResult<()> {
        if !self.trades.contains_key(id) {
            return Err(OmsError::UnknownTrade(id.to_string()));
        }
        if self.begin_exit(id, reason, now) {
            self.advance(id, now).await?;
            self.sync(now).await?;
        }
        Ok(())
    }

    /// Exits every live trade.
    pub async fn flatten_all(&mut self, reason: &str, now: Ts) -> OmsResult<()> {
        let live: Vec<String> = self.live_trades().map(|t| t.id.clone()).collect();
        for id in &live {
            if self.begin_exit(id, reason, now) {
                self.advance(id, now).await?;
            }
        }
        self.sync(now).await
    }

    /// Engine-managed targets: exits trades on `token` whose target `ltp`
    /// has reached. Returns the trades it closed out.
    pub async fn on_ltp(&mut self, token: &str, ltp: Price, now: Ts) -> OmsResult<Vec<String>> {
        let hit: Vec<String> = self
            .live_trades()
            .filter(|t| t.token == token && t.state == TradeState::Open)
            .filter(|t| match (t.target, t.side) {
                (Some(x), Side::Buy) => ltp >= x,
                (Some(x), Side::Sell) => ltp <= x,
                (None, _) => false,
            })
            .map(|t| t.id.clone())
            .collect();
        for id in &hit {
            self.request_exit(id, "target", now).await?;
        }
        Ok(hit)
    }

    /// Positions the broker still holds (non-zero), e.g. to verify the
    /// square-off.
    pub async fn broker_open_positions(&self) -> OmsResult<Vec<Position>> {
        Ok(self
            .broker
            .positions()
            .await?
            .into_iter()
            .filter(|p| p.net_qty != 0)
            .collect())
    }

    /// Compares the broker's positions with the live trades, symbol by
    /// symbol. A difference must show on two checks in a row (fills race
    /// with the two reads). The broker is the truth:
    ///
    /// * it holds **more** than we know (or the opposite side): an orphan
    ///   trade is created for the excess and exited at market, and the
    ///   result is `severe` so the engine halts;
    /// * it holds **less**: the missing quantity was closed behind our back
    ///   (e.g. the broker's own square-off); the trade is marked exited.
    pub async fn reconcile(&mut self, now: Ts) -> OmsResult<Reconciliation> {
        self.sync(now).await?;
        let positions = self.broker.positions().await?;
        let mut local: BTreeMap<Symbol, i64> = BTreeMap::new();
        for t in self.live_trades() {
            *local.entry(t.symbol.clone()).or_default() += t.net_qty();
        }
        let mut broker: BTreeMap<Symbol, &Position> = BTreeMap::new();
        for p in &positions {
            broker.insert(p.symbol.clone(), p);
        }
        let symbols: HashSet<Symbol> = local.keys().chain(broker.keys()).cloned().collect();
        let mut out = Reconciliation::default();
        let mut seen = HashMap::new();
        for sym in symbols {
            let l = local.get(&sym).copied().unwrap_or(0);
            let b = broker.get(&sym).map_or(0, |p| p.net_qty);
            if l == b {
                continue;
            }
            seen.insert(sym.clone(), (l, b));
            if self.suspect.get(&sym) != Some(&(l, b)) {
                continue;
            }
            let msg = format!("{sym}: local {l}, broker {b}");
            out.mismatches.push(msg.clone());
            let more = b.signum() != l.signum() && b != 0 || b.abs() > l.abs();
            if more {
                out.severe = true;
                self.alert(
                    now,
                    Severity::Critical,
                    format!("reconcile: {msg}; exiting the unknown quantity"),
                );
                let excess = b - if b.signum() == l.signum() { l } else { 0 };
                let template = broker.get(&sym).map(|p| (*p).clone());
                self.adopt_orphan(&sym, excess, template, now).await?;
            } else {
                self.alert(
                    now,
                    Severity::Warning,
                    format!("reconcile: {msg}; marking the difference as exited"),
                );
                self.write_off(&sym, l, b, now);
            }
        }
        self.suspect = seen;
        if !out.mismatches.is_empty() {
            self.sync(now).await?;
        }
        self.flush()?;
        Ok(out)
    }

    /// Creates a trade for a position the broker holds that we did not know
    /// about, and exits it.
    async fn adopt_orphan(&mut self, sym: &Symbol, excess: i64, pos: Option<Position>, now: Ts) -> OmsResult<()> {
        let side = if excess > 0 { Side::Buy } else { Side::Sell };
        let qty = excess.unsigned_abs() as Qty;
        // Token/exchange from any trade we had on the symbol.
        let known = self.trades.values().find(|t| &t.symbol == sym).cloned();
        let Some(k) = known else {
            self.alert(
                now,
                Severity::Critical,
                format!("reconcile: no instrument details for {sym}; flatten it by hand"),
            );
            return Ok(());
        };
        let avg = pos.as_ref().map_or(k.signal_px, |p| p.avg_price);
        let id = self.next_trade_id();
        let t = Trade {
            id: id.clone(),
            day: MarketClock::ist_date(now),
            strategy: "orphan".into(),
            planned_qty: qty,
            signal_px: avg,
            signal_ts: now,
            side,
            stop: avg,
            target: None,
            planned_risk: Price::ZERO,
            reason: "found at the broker during reconciliation".into(),
            state: TradeState::Exiting,
            entry_qty: qty,
            entry_value: avg.0 * i64::from(qty),
            exit_qty: 0,
            exit_value: 0,
            charges: Price::ZERO,
            exit_reason: Some("reconcile_orphan".into()),
            opened_at: Some(now),
            closed_at: None,
            orders: Vec::new(),
            exit_failures: 0,
            ..k
        };
        self.trades.insert(id.clone(), t);
        self.touch_trade(&id);
        self.advance(&id, now).await
    }

    /// The broker holds less than we think: book the difference as exited
    /// at the average entry (P&L unknown, so zero) on the newest trades.
    fn write_off(&mut self, sym: &Symbol, local: i64, broker: i64, now: Ts) {
        let mut gone = (local.abs() - broker.abs()).max(0) as Qty;
        if local.signum() != broker.signum() && broker != 0 {
            gone = local.unsigned_abs() as Qty;
        }
        let mut ids: Vec<String> = self
            .live_trades()
            .filter(|t| &t.symbol == sym && t.open_qty() > 0)
            .map(|t| t.id.clone())
            .collect();
        ids.reverse();
        for id in ids {
            if gone == 0 {
                break;
            }
            let t = self.trades.get_mut(&id).expect("trade");
            let q = gone.min(t.open_qty());
            let px = t.avg_entry().unwrap_or(t.signal_px);
            t.exit_qty += q;
            t.exit_value += px.0 * i64::from(q);
            if t.exit_reason.is_none() {
                t.exit_reason = Some("closed_at_broker".into());
            }
            gone -= q;
            self.touch_trade(&id);
            if self.trades[&id].open_qty() == 0 {
                // Its stop (if any) gets cancelled by the state machine.
                let t = self.trades.get_mut(&id).expect("trade");
                if t.state == TradeState::Open {
                    t.state = TradeState::Exiting;
                }
            }
        }
        let _ = now;
    }
}
