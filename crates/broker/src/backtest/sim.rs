//! A deterministic simulated exchange driven by historical bars.
//!
//! Fill rules, chosen to be pessimistic where a bar hides the order of
//! events:
//!
//! * An order placed at time `t` can only fill on bars that start at or
//!   after `t`. A signal computed at a bar's close therefore fills on the
//!   next bar, never on the bar that produced it (no look-ahead).
//! * Market orders fill at the bar's open, plus slippage.
//! * Stop orders trigger when the bar trades through the trigger. If the bar
//!   opens beyond the trigger (a gap), the fill is the open, not the trigger.
//!   Slippage applies.
//! * Limit orders fill at the limit once price trades through it by
//!   [`Slippage::limit_through_ticks`]; at the open if it gaps through.
//! * If a stop and a target of the same bracket could both have filled in one
//!   bar, the **stop** wins, unless the open alone settles it.
//! * Fills are clamped to the bar's high–low range.
//!
//! Every fill pays the [`CostModel`] charges.

use std::collections::{BTreeMap, HashMap};

use saarthi_core::{Bar, Price, Qty, Side, Ts};

use super::costs::{Charges, CostModel, Slippage};
use crate::error::{BrokerError, BrokerResult};
use crate::types::{BrokerOrder, BrokerOrderId, Modify, OrderKind, OrderRequest, OrderStatus, Product};

/// Why an order filled, for the trade log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillKind {
    Market,
    Stop,
    Limit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimFill {
    pub order_id: BrokerOrderId,
    pub client_order_id: String,
    pub token: String,
    pub side: Side,
    pub qty: Qty,
    pub price: Price,
    pub ts: Ts,
    pub kind: FillKind,
    pub charges: Charges,
}

#[derive(Debug, Clone)]
struct SimOrder {
    seq: u64,
    req: OrderRequest,
    placed_at: Ts,
    status: OrderStatus,
    /// Children of a bracket wait for their parent to fill.
    active: bool,
    parent: Option<u64>,
    /// Orders sharing a group cancel each other when one fills.
    oco: Option<u64>,
    fill: Option<(Price, Ts)>,
}

/// Net position in one instrument.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimPosition {
    /// Positive long, negative short.
    pub qty: i64,
    /// Total entry cost of the open quantity, in paise.
    pub cost: i128,
    /// Realized P&L before charges.
    pub realized: Price,
    pub charges: Price,
    /// Last traded price seen (bar close) for marking.
    pub mark: Price,
}

impl SimPosition {
    pub fn avg_price(&self) -> Option<Price> {
        (self.qty != 0).then(|| Price((self.cost / i128::from(self.qty.abs())) as i64))
    }

    pub fn unrealized(&self) -> Price {
        if self.qty == 0 {
            return Price::ZERO;
        }
        let value = i128::from(self.mark.0) * i128::from(self.qty.abs());
        let pnl = if self.qty > 0 {
            value - self.cost
        } else {
            self.cost - value
        };
        Price(pnl as i64)
    }

    fn apply(&mut self, side: Side, qty: Qty, price: Price) {
        let signed = match side {
            Side::Buy => i64::from(qty),
            Side::Sell => -i64::from(qty),
        };
        let q = i128::from(qty);
        let p = i128::from(price.0);
        if self.qty == 0 || self.qty.signum() == signed.signum() {
            self.cost += p * q;
            self.qty += signed;
            return;
        }
        // Reducing (and maybe flipping).
        let open = i128::from(self.qty.abs());
        let closing = q.min(open);
        let avg_cost = self.cost * closing / open;
        let proceeds = p * closing;
        let pnl = if self.qty > 0 {
            proceeds - avg_cost
        } else {
            avg_cost - proceeds
        };
        self.realized = self.realized + Price(pnl as i64);
        self.cost -= avg_cost;
        self.qty += signed;
        if q > closing {
            self.cost = p * (q - closing);
        }
        if self.qty == 0 {
            self.cost = 0;
        }
    }
}

/// The simulated exchange plus account.
#[derive(Debug, Clone)]
pub struct SimExchange {
    costs: CostModel,
    slippage: Slippage,
    capital: Price,
    seq: u64,
    orders: BTreeMap<u64, SimOrder>,
    positions: HashMap<String, SimPosition>,
    total_charges: Charges,
    now: Ts,
    /// The latest request per token, as a template for exit orders.
    last_req: HashMap<String, OrderRequest>,
}

fn id_of(seq: u64) -> BrokerOrderId {
    BrokerOrderId(format!("BT{seq:08}"))
}

fn seq_of(id: &BrokerOrderId) -> BrokerResult<u64> {
    id.0.strip_prefix("BT")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| BrokerError::Api {
            code: "BT404".into(),
            message: format!("unknown order id {}", id.0),
        })
}

fn reject(msg: impl Into<String>) -> BrokerError {
    BrokerError::Api {
        code: "BT400".into(),
        message: msg.into(),
    }
}

impl SimExchange {
    pub fn new(costs: CostModel, slippage: Slippage, capital: Price) -> Self {
        SimExchange {
            costs,
            slippage,
            capital,
            seq: 0,
            orders: BTreeMap::new(),
            positions: HashMap::new(),
            total_charges: Charges::default(),
            now: Ts::default(),
            last_req: HashMap::new(),
        }
    }

    pub fn costs(&self) -> &CostModel {
        &self.costs
    }

    pub fn slippage(&self) -> &Slippage {
        &self.slippage
    }

    /// The replay clock: orders placed now become eligible for bars that
    /// start at or after it.
    pub fn now(&self) -> Ts {
        self.now
    }

    pub fn set_time(&mut self, now: Ts) {
        self.now = self.now.max(now);
    }

    fn validate(&self, req: &OrderRequest) -> BrokerResult<()> {
        if req.qty == 0 {
            return Err(reject("quantity must be positive"));
        }
        if req.product != Product::Intraday {
            return Err(reject("the backtester only simulates intraday (MIS) orders"));
        }
        let bad_px = |p: Price| p.0 <= 0;
        match req.kind {
            OrderKind::Market => {}
            OrderKind::Limit { price } if bad_px(price) => return Err(reject("limit price must be positive")),
            OrderKind::StopLossMarket { trigger } if bad_px(trigger) => {
                return Err(reject("trigger must be positive"));
            }
            OrderKind::StopLossLimit { .. } => return Err(reject("SL-L is not simulated; use SL-M")),
            _ => {}
        }
        Ok(())
    }

    fn insert(&mut self, req: OrderRequest, active: bool, parent: Option<u64>, oco: Option<u64>) -> u64 {
        self.seq += 1;
        let seq = self.seq;
        self.last_req.insert(req.token.clone(), req.clone());
        self.orders.insert(
            seq,
            SimOrder {
                seq,
                req,
                placed_at: self.now,
                status: if active { OrderStatus::Open } else { OrderStatus::New },
                active,
                parent,
                oco,
                fill: None,
            },
        );
        seq
    }

    pub fn place(&mut self, req: OrderRequest) -> BrokerResult<BrokerOrderId> {
        self.validate(&req)?;
        Ok(id_of(self.insert(req, true, None, None)))
    }

    /// An entry with a protective stop and an optional target that go live
    /// when the entry fills and cancel each other (one-cancels-other).
    /// Returns `(entry, stop, target)` ids.
    pub fn place_bracket(
        &mut self,
        entry: OrderRequest,
        stop: Price,
        target: Option<Price>,
    ) -> BrokerResult<(BrokerOrderId, BrokerOrderId, Option<BrokerOrderId>)> {
        self.validate(&entry)?;
        let ok = match entry.side {
            Side::Buy => target.is_none_or(|t| t > stop),
            Side::Sell => target.is_none_or(|t| t < stop),
        };
        if stop.0 <= 0 || !ok {
            return Err(reject("bracket stop/target are on the wrong side"));
        }
        let exit_side = match entry.side {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        };
        let child = |suffix: &str, kind| OrderRequest {
            client_order_id: format!("{}-{suffix}", entry.client_order_id),
            side: exit_side,
            kind,
            ..entry.clone()
        };
        let stop_req = child("sl", OrderKind::StopLossMarket { trigger: stop });
        let target_req = target.map(|t| child("tp", OrderKind::Limit { price: t }));
        let e = self.insert(entry, true, None, None);
        let s = self.insert(stop_req, false, Some(e), Some(e));
        let t = target_req.map(|r| self.insert(r, false, Some(e), Some(e)));
        Ok((id_of(e), id_of(s), t.map(id_of)))
    }

    pub fn cancel(&mut self, id: &BrokerOrderId) -> BrokerResult<()> {
        let seq = seq_of(id)?;
        let o = self
            .orders
            .get_mut(&seq)
            .ok_or_else(|| reject(format!("unknown order {}", id.0)))?;
        if matches!(
            o.status,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected
        ) {
            return Err(reject(format!("order {} is already {:?}", id.0, o.status)));
        }
        o.status = OrderStatus::Cancelled;
        // Children of an unfilled entry die with it.
        for c in self.orders.values_mut().filter(|c| c.parent == Some(seq)) {
            if matches!(c.status, OrderStatus::New | OrderStatus::Open) {
                c.status = OrderStatus::Cancelled;
            }
        }
        Ok(())
    }

    pub fn modify(&mut self, id: &BrokerOrderId, m: Modify) -> BrokerResult<()> {
        let seq = seq_of(id)?;
        let o = self
            .orders
            .get_mut(&seq)
            .ok_or_else(|| reject(format!("unknown order {}", id.0)))?;
        if !matches!(o.status, OrderStatus::New | OrderStatus::Open) {
            return Err(reject(format!("order {} is {:?}", id.0, o.status)));
        }
        if let Some(q) = m.qty {
            if q == 0 {
                return Err(reject("quantity must be positive"));
            }
            o.req.qty = q;
        }
        match (&mut o.req.kind, m.price, m.trigger) {
            (OrderKind::Limit { price }, Some(p), _) => *price = p,
            (OrderKind::StopLossMarket { trigger }, _, Some(t)) => *trigger = t,
            (_, None, None) => {}
            _ => return Err(reject("modify does not match the order type")),
        }
        Ok(())
    }

    /// Cancels every working order for `token` and, if a position is open,
    /// sends a market order to close it. Returns the exit order id.
    pub fn flatten(&mut self, token: &str, tag: &str) -> Option<BrokerOrderId> {
        let ids: Vec<u64> = self
            .orders
            .values()
            .filter(|o| o.req.token == token && matches!(o.status, OrderStatus::New | OrderStatus::Open))
            .map(|o| o.seq)
            .collect();
        for seq in ids {
            if let Some(o) = self.orders.get_mut(&seq) {
                o.status = OrderStatus::Cancelled;
            }
        }
        let pos = self.positions.get(token).copied().unwrap_or_default();
        if pos.qty == 0 {
            return None;
        }
        let template = self.last_req.get(token)?.clone();
        let req = OrderRequest {
            client_order_id: format!("{tag}-{}", self.seq + 1),
            side: if pos.qty > 0 { Side::Sell } else { Side::Buy },
            qty: pos.qty.unsigned_abs() as Qty,
            kind: OrderKind::Market,
            tag: tag.to_string(),
            ..template
        };
        Some(id_of(self.insert(req, true, None, None)))
    }

    /// Closes `token`'s position immediately at `price` (with slippage),
    /// e.g. when the data ends before a square-off order could fill.
    pub fn force_close(&mut self, token: &str, price: Price, ts: Ts) -> Option<SimFill> {
        let pos = self.positions.get(token).copied().unwrap_or_default();
        if pos.qty == 0 {
            return None;
        }
        for o in self.orders.values_mut().filter(|o| o.req.token == token) {
            if matches!(o.status, OrderStatus::New | OrderStatus::Open) {
                o.status = OrderStatus::Cancelled;
            }
        }
        let side = if pos.qty > 0 { Side::Sell } else { Side::Buy };
        let qty = pos.qty.unsigned_abs() as Qty;
        let px = self.slippage.adverse(side, price);
        let template = self.last_req.get(token)?.clone();
        let seq = self.insert(
            OrderRequest {
                client_order_id: format!("force-{}", self.seq + 1),
                side,
                qty,
                kind: OrderKind::Market,
                ..template
            },
            true,
            None,
            None,
        );
        Some(self.execute(seq, px, ts, FillKind::Market))
    }

    fn execute(&mut self, seq: u64, price: Price, ts: Ts, kind: FillKind) -> SimFill {
        let o = self.orders.get_mut(&seq).expect("order exists");
        o.status = OrderStatus::Filled;
        o.fill = Some((price, ts));
        let req = o.req.clone();
        let charges = self.costs.charges(req.side, price, req.qty);
        self.total_charges += charges;
        let pos = self.positions.entry(req.token.clone()).or_default();
        pos.apply(req.side, req.qty, price);
        pos.charges = pos.charges + charges.total();
        // Brackets: arm the children, or cancel the siblings.
        let filled_qty = req.qty;
        let oco = o.oco;
        for c in self.orders.values_mut() {
            if c.parent == Some(seq) && c.status == OrderStatus::New {
                c.active = true;
                c.status = OrderStatus::Open;
                c.req.qty = filled_qty;
                c.placed_at = ts;
            } else if oco.is_some() && c.oco == oco && c.seq != seq && matches!(c.status, OrderStatus::Open) {
                c.status = OrderStatus::Cancelled;
            }
        }
        SimFill {
            order_id: id_of(seq),
            client_order_id: req.client_order_id,
            token: req.token,
            side: req.side,
            qty: req.qty,
            price,
            ts,
            kind,
            charges,
        }
    }

    fn clamp(bar: &Bar, p: Price) -> Price {
        p.max(bar.low).min(bar.high)
    }

    fn working(&self, token: &str, bar: &Bar, f: impl Fn(&OrderKind) -> bool) -> Vec<u64> {
        self.orders
            .values()
            .filter(|o| {
                o.req.token == token
                    && o.active
                    && o.status == OrderStatus::Open
                    && o.placed_at <= bar.ts
                    && f(&o.req.kind)
            })
            .map(|o| o.seq)
            .collect()
    }

    /// Where a resting order would fill in `bar`: `(price, at_open)`.
    fn resting_fill(&self, o: &SimOrder, bar: &Bar) -> Option<(Price, bool, FillKind)> {
        let tick = self.slippage.tick();
        match (o.req.kind, o.req.side) {
            (OrderKind::StopLossMarket { trigger }, Side::Sell) => {
                if bar.open <= trigger {
                    Some((self.slippage.adverse(Side::Sell, bar.open), true, FillKind::Stop))
                } else if bar.low <= trigger {
                    Some((self.slippage.adverse(Side::Sell, trigger), false, FillKind::Stop))
                } else {
                    None
                }
            }
            (OrderKind::StopLossMarket { trigger }, Side::Buy) => {
                if bar.open >= trigger {
                    Some((self.slippage.adverse(Side::Buy, bar.open), true, FillKind::Stop))
                } else if bar.high >= trigger {
                    Some((self.slippage.adverse(Side::Buy, trigger), false, FillKind::Stop))
                } else {
                    None
                }
            }
            (OrderKind::Limit { price }, Side::Sell) => {
                let through = Price(price.0 + self.slippage.limit_through_ticks * tick);
                if bar.open >= price {
                    Some((bar.open, true, FillKind::Limit))
                } else if bar.high >= through {
                    Some((price, false, FillKind::Limit))
                } else {
                    None
                }
            }
            (OrderKind::Limit { price }, Side::Buy) => {
                let through = Price(price.0 - self.slippage.limit_through_ticks * tick);
                if bar.open <= price {
                    Some((bar.open, true, FillKind::Limit))
                } else if bar.low <= through {
                    Some((price, false, FillKind::Limit))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Plays one bar of `token` against the working orders. Returns the
    /// fills in the order they happened.
    pub fn on_bar(&mut self, token: &str, bar: &Bar) -> Vec<SimFill> {
        let mut fills = Vec::new();
        // 1. Market orders at the open.
        for seq in self.working(token, bar, |k| matches!(k, OrderKind::Market)) {
            let side = self.orders[&seq].req.side;
            let px = Self::clamp(bar, self.slippage.adverse(side, bar.open));
            fills.push(self.execute(seq, px, bar.ts, FillKind::Market));
        }
        // 2. Resting stops and limits (including brackets armed just now).
        //    Settle each OCO group once; prefer what the open decides, then
        //    the stop.
        let resting = self.working(token, bar, |k| !matches!(k, OrderKind::Market));
        let mut groups: BTreeMap<u64, Vec<(u64, Price, bool, FillKind)>> = BTreeMap::new();
        for seq in resting {
            let o = &self.orders[&seq];
            if let Some((px, at_open, kind)) = self.resting_fill(o, bar) {
                groups
                    .entry(o.oco.unwrap_or(u64::MAX - seq))
                    .or_default()
                    .push((seq, px, at_open, kind));
            }
        }
        for (_, mut cands) in groups {
            cands.sort_by_key(|&(seq, _, at_open, kind)| (!at_open, kind != FillKind::Stop, seq));
            let (seq, px, _, kind) = cands[0];
            if self.orders[&seq].status != OrderStatus::Open {
                continue;
            }
            let px = Self::clamp(bar, px);
            fills.push(self.execute(seq, px, bar.ts, kind));
        }
        let pos = self.positions.entry(token.to_string()).or_default();
        pos.mark = bar.close;
        fills
    }

    pub fn position(&self, token: &str) -> SimPosition {
        self.positions.get(token).copied().unwrap_or_default()
    }

    pub fn positions(&self) -> impl Iterator<Item = (&str, &SimPosition)> {
        self.positions.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn total_charges(&self) -> Charges {
        self.total_charges
    }

    pub fn realized(&self) -> Price {
        self.positions.values().map(|p| p.realized).sum()
    }

    pub fn unrealized(&self) -> Price {
        self.positions.values().map(SimPosition::unrealized).sum()
    }

    /// Capital + realized + unrealized − charges.
    pub fn equity(&self) -> Price {
        self.capital + self.realized() + self.unrealized() - self.total_charges.total()
    }

    pub fn order(&self, id: &BrokerOrderId) -> Option<BrokerOrder> {
        let seq = seq_of(id).ok()?;
        self.orders.get(&seq).map(to_broker_order)
    }

    pub fn order_book(&self) -> Vec<BrokerOrder> {
        self.orders.values().map(to_broker_order).collect()
    }

    /// Number of orders still able to fill.
    pub fn working_count(&self, token: &str) -> usize {
        self.orders
            .values()
            .filter(|o| o.req.token == token && matches!(o.status, OrderStatus::New | OrderStatus::Open))
            .count()
    }

    /// Drops finished orders to keep long replays lean.
    pub fn prune(&mut self) {
        self.orders
            .retain(|_, o| matches!(o.status, OrderStatus::New | OrderStatus::Open));
    }
}

fn to_broker_order(o: &SimOrder) -> BrokerOrder {
    BrokerOrder {
        id: id_of(o.seq),
        client_order_id: Some(o.req.client_order_id.clone()),
        symbol: o.req.symbol.clone(),
        side: o.req.side,
        qty: o.req.qty,
        filled_qty: if o.status == OrderStatus::Filled { o.req.qty } else { 0 },
        avg_price: o.fill.map(|f| f.0),
        status: o.status,
        reject_reason: None,
    }
}
