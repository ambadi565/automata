//! `Paper`: a simulated broker driven by the **live** feed.
//!
//! The engine runs exactly as it would live (same OMS, same risk guard, same
//! order flow through the [`Broker`] trait); only the exchange is simulated:
//!
//! * Market orders fill on the next tick of their symbol at the LTP plus
//!   [`Slippage`], never at a price seen before the order existed.
//! * SL-M orders trigger when the LTP trades through the trigger and fill at
//!   the LTP plus slippage (so gaps fill worse than the trigger). A stop
//!   placed on the wrong side of the LTP is rejected, as the exchange would.
//! * Limit orders marketable on arrival fill at the LTP; resting ones fill at
//!   their price once the LTP trades [`Slippage::limit_through_ticks`]
//!   through it.
//! * Optional partial fills: at most `max_fill_qty_per_tick` per tick.
//! * Every fill pays the [`CostModel`] charges.
//!
//! Like a real broker, the account outlives the engine: with a state file
//! the order book, positions and cash are written after every change, so an
//! engine crash and restart finds the "broker" exactly where it was.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use saarthi_core::{Bar, Exchange, IST, Instrument, Price, Qty, Side, Symbol, Tf, Tick, Ts};
use serde::{Deserialize, Serialize};

use crate::Broker;
use crate::backtest::{CostModel, Slippage};
use crate::error::{BrokerError, BrokerResult};
use crate::secret::Secret;
use crate::types::{
    BrokerOrder, BrokerOrderId, Funds, Modify, OrderKind, OrderRequest, OrderStatus, Position, Product, Session,
};

#[derive(Debug, Clone)]
pub struct PaperConfig {
    /// Starting cash for a fresh account.
    pub capital: Price,
    pub costs: CostModel,
    pub slippage: Slippage,
    /// Partial fills: at most this many shares fill per tick.
    pub max_fill_qty_per_tick: Option<Qty>,
    /// Persist the account here (JSON, written atomically).
    pub state_path: Option<PathBuf>,
}

/// One paper fill, for logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaperFill {
    pub order_id: BrokerOrderId,
    pub client_order_id: String,
    pub token: String,
    pub side: Side,
    pub qty: Qty,
    pub price: Price,
    pub ts: Ts,
    pub charges: Price,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaperOrder {
    seq: u64,
    req: OrderRequest,
    status: OrderStatus,
    filled: Qty,
    /// Σ price × qty of fills, for the average.
    value: i64,
    reject_reason: Option<String>,
    /// Has seen at least one tick since it was placed (a limit order that
    /// was not marketable on arrival is resting).
    rested: bool,
}

impl PaperOrder {
    fn working(&self) -> bool {
        matches!(self.status, OrderStatus::Open | OrderStatus::PartiallyFilled)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PaperPosition {
    symbol: Symbol,
    exchange: Option<Exchange>,
    qty: i64,
    /// Σ entry price × qty of the open quantity.
    cost: i64,
    realized: Price,
    /// Last traded price seen, for the broker's own square-off at a day roll.
    #[serde(default)]
    mark: Option<Price>,
}

impl PaperPosition {
    fn apply(&mut self, side: Side, qty: Qty, price: Price) {
        let signed = side.sign() * i64::from(qty);
        if self.qty == 0 || self.qty.signum() == signed.signum() {
            self.cost += price.0 * i64::from(qty);
            self.qty += signed;
            return;
        }
        let open = self.qty.abs();
        let closing = i64::from(qty).min(open);
        let avg_cost = self.cost * closing / open;
        let proceeds = price.0 * closing;
        let pnl = if self.qty > 0 {
            proceeds - avg_cost
        } else {
            avg_cost - proceeds
        };
        self.realized = self.realized + Price(pnl);
        self.cost -= avg_cost;
        self.qty += signed;
        if i64::from(qty) > closing {
            self.cost = price.0 * (i64::from(qty) - closing);
        }
        if self.qty == 0 {
            self.cost = 0;
        }
    }

    fn unrealized(&self, mark: Option<Price>) -> Price {
        match mark {
            Some(m) if self.qty != 0 => {
                let value = m.0 * self.qty.abs();
                Price(if self.qty > 0 {
                    value - self.cost
                } else {
                    self.cost - value
                })
            }
            _ => Price::ZERO,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Account {
    /// Cash at the start of the trading day.
    cash: Price,
    day: Option<NaiveDate>,
    seq: u64,
    orders: BTreeMap<u64, PaperOrder>,
    positions: BTreeMap<String, PaperPosition>,
    charges: Price,
}

struct Inner {
    acct: Account,
    ltp: HashMap<String, Price>,
}

#[derive(Clone)]
pub struct Paper {
    cfg: Arc<PaperConfig>,
    inner: Arc<Mutex<Inner>>,
}

fn id_of(seq: u64) -> BrokerOrderId {
    BrokerOrderId(format!("PAPER{seq:08}"))
}

fn seq_of(id: &BrokerOrderId) -> BrokerResult<u64> {
    id.0.strip_prefix("PAPER")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| api("PP404", format!("unknown order id {}", id.0)))
}

fn api(code: &str, message: impl Into<String>) -> BrokerError {
    BrokerError::Api {
        code: code.into(),
        message: message.into(),
    }
}

impl Paper {
    /// Opens the paper account: loads the state file if it exists, otherwise
    /// starts fresh with `cfg.capital`.
    pub fn open(cfg: PaperConfig) -> BrokerResult<Self> {
        let acct = match &cfg.state_path {
            Some(p) if p.exists() => {
                let s = std::fs::read_to_string(p)
                    .map_err(|e| BrokerError::Config(format!("reading {}: {e}", p.display())))?;
                serde_json::from_str(&s).map_err(|e| BrokerError::Config(format!("{}: {e}", p.display())))?
            }
            _ => Account {
                cash: cfg.capital,
                ..Account::default()
            },
        };
        Ok(Paper {
            cfg: Arc::new(cfg),
            inner: Arc::new(Mutex::new(Inner {
                acct,
                ltp: HashMap::new(),
            })),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, acct: &Account) -> BrokerResult<()> {
        let Some(path) = &self.cfg.state_path else {
            return Ok(());
        };
        let json = serde_json::to_vec_pretty(acct).map_err(|e| BrokerError::Decode(e.to_string()))?;
        write_atomic(path, &json).map_err(|e| BrokerError::Config(format!("writing {}: {e}", path.display())))
    }

    pub fn ltp(&self, token: &str) -> Option<Price> {
        self.lock().ltp.get(token).copied()
    }

    /// Plays one live tick against the working orders of its symbol.
    pub fn on_tick(&self, t: &Tick) -> BrokerResult<Vec<PaperFill>> {
        let mut g = self.lock();
        g.ltp.insert(t.token.clone(), t.ltp);
        let day = t.ts.with_timezone(&IST).date_naive();
        let mut changed = roll_day(&mut g.acct, day);
        if let Some(p) = g.acct.positions.get_mut(&t.token) {
            p.mark = Some(t.ltp);
        }
        let mut fills = Vec::new();
        let seqs: Vec<u64> = g
            .acct
            .orders
            .values()
            .filter(|o| o.req.token == t.token && o.working())
            .map(|o| o.seq)
            .collect();
        for seq in seqs {
            let o = &g.acct.orders[&seq];
            let px = self.fill_price(o, t.ltp);
            let rested = o.rested;
            if let Some(px) = px {
                let f = self.execute(&mut g.acct, seq, px, t.ts);
                fills.push(f);
                changed = true;
            } else if !rested {
                g.acct.orders.get_mut(&seq).expect("order").rested = true;
            }
        }
        if changed {
            self.persist(&g.acct)?;
        }
        Ok(fills)
    }

    fn fill_price(&self, o: &PaperOrder, ltp: Price) -> Option<Price> {
        let s = &self.cfg.slippage;
        let through = s.limit_through_ticks * s.tick();
        match (o.req.kind, o.req.side) {
            (OrderKind::Market, side) => Some(s.adverse(side, ltp)),
            (OrderKind::StopLossMarket { trigger }, Side::Sell) => (ltp <= trigger).then(|| s.adverse(Side::Sell, ltp)),
            (OrderKind::StopLossMarket { trigger }, Side::Buy) => (ltp >= trigger).then(|| s.adverse(Side::Buy, ltp)),
            (OrderKind::Limit { price }, Side::Buy) if !o.rested => (ltp <= price).then_some(ltp),
            (OrderKind::Limit { price }, Side::Sell) if !o.rested => (ltp >= price).then_some(ltp),
            (OrderKind::Limit { price }, Side::Buy) => (ltp.0 <= price.0 - through).then_some(price),
            (OrderKind::Limit { price }, Side::Sell) => (ltp.0 >= price.0 + through).then_some(price),
            (OrderKind::StopLossLimit { .. }, _) => None,
        }
    }

    fn execute(&self, acct: &mut Account, seq: u64, price: Price, ts: Ts) -> PaperFill {
        let o = acct.orders.get_mut(&seq).expect("order exists");
        let remaining = o.req.qty - o.filled;
        let qty = self
            .cfg
            .max_fill_qty_per_tick
            .map_or(remaining, |m| remaining.min(m.max(1)));
        o.filled += qty;
        o.value += price.0 * i64::from(qty);
        o.status = if o.filled == o.req.qty {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        let req = o.req.clone();
        let charges = self.cfg.costs.charges(req.side, price, qty).total();
        acct.charges = acct.charges + charges;
        let pos = acct.positions.entry(req.token.clone()).or_default();
        pos.symbol = req.symbol.clone();
        pos.exchange = Some(req.exchange);
        pos.apply(req.side, qty, price);
        PaperFill {
            order_id: id_of(seq),
            client_order_id: req.client_order_id,
            token: req.token,
            side: req.side,
            qty,
            price,
            ts,
            charges,
        }
    }

    fn equity(&self, g: &Inner) -> Price {
        let realized: Price = g.acct.positions.values().map(|p| p.realized).sum();
        let unrealized: Price = g
            .acct
            .positions
            .iter()
            .map(|(tok, p)| p.unrealized(g.ltp.get(tok).copied()))
            .sum();
        g.acct.cash + realized + unrealized - g.acct.charges
    }

    /// Charges paid today.
    pub fn charges(&self) -> Price {
        self.lock().acct.charges
    }
}

/// A new trading day: the broker cancels what is left of yesterday's
/// orders and books yesterday's P&L into cash.
fn roll_day(acct: &mut Account, day: NaiveDate) -> bool {
    if acct.day == Some(day) {
        return false;
    }
    if acct.day.is_some() {
        let realized: Price = acct.positions.values().map(|p| p.realized).sum();
        acct.cash = acct.cash + realized - acct.charges;
        if acct.positions.values().any(|p| p.qty != 0) {
            tracing::warn!("paper: positions left open overnight were squared off at their last price");
        }
        acct.charges = Price::ZERO;
        // Intraday positions cannot survive the night: the broker squares
        // off anything left at its last price.
        let leftover: Price = acct.positions.values().map(|p| p.unrealized(p.mark)).sum();
        acct.cash = acct.cash + leftover;
        acct.orders.clear();
        acct.positions.clear();
    }
    acct.day = Some(day);
    true
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("paper"),
        std::process::id()
    ));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[async_trait]
impl Broker for Paper {
    async fn login(&self) -> BrokerResult<Session> {
        Ok(Session {
            client_code: "PAPER".into(),
            jwt: Secret::new(String::new()),
            refresh_token: Secret::new(String::new()),
            feed_token: Secret::new(String::new()),
            created_at: Utc::now(),
        })
    }

    async fn place(&self, req: OrderRequest) -> BrokerResult<BrokerOrderId> {
        if req.qty == 0 {
            return Err(api("PP400", "quantity must be positive"));
        }
        if req.product != Product::Intraday {
            return Err(api("PP400", "paper trading simulates intraday (MIS) orders only"));
        }
        let positive = |p: Price| p.0 > 0;
        let shape_ok = match req.kind {
            OrderKind::Market => true,
            OrderKind::Limit { price } => positive(price),
            OrderKind::StopLossMarket { trigger } => positive(trigger),
            OrderKind::StopLossLimit { .. } => return Err(api("PP400", "SL-L is not simulated; use SL-M")),
        };
        if !shape_ok {
            return Err(api("PP400", "prices must be positive"));
        }
        let mut g = self.lock();
        // Like the exchange, a stop already through the market is accepted
        // and then rejected.
        let reject_reason = match (req.kind, req.side, g.ltp.get(&req.token)) {
            (OrderKind::StopLossMarket { trigger }, Side::Sell, Some(&ltp)) if trigger >= ltp => {
                Some(format!("trigger {trigger} is not below LTP {ltp}"))
            }
            (OrderKind::StopLossMarket { trigger }, Side::Buy, Some(&ltp)) if trigger <= ltp => {
                Some(format!("trigger {trigger} is not above LTP {ltp}"))
            }
            _ => None,
        };
        g.acct.seq += 1;
        let seq = g.acct.seq;
        g.acct.orders.insert(
            seq,
            PaperOrder {
                seq,
                req,
                status: if reject_reason.is_some() {
                    OrderStatus::Rejected
                } else {
                    OrderStatus::Open
                },
                filled: 0,
                value: 0,
                reject_reason,
                rested: false,
            },
        );
        self.persist(&g.acct)?;
        Ok(id_of(seq))
    }

    async fn modify(&self, id: &BrokerOrderId, m: Modify) -> BrokerResult<()> {
        let seq = seq_of(id)?;
        let mut g = self.lock();
        let o = g
            .acct
            .orders
            .get_mut(&seq)
            .ok_or_else(|| api("PP404", format!("unknown order {}", id.0)))?;
        if !o.working() {
            return Err(api("PP409", format!("order {} is {}", id.0, o.status.as_str())));
        }
        if let Some(q) = m.qty {
            if q <= o.filled {
                return Err(api("PP400", "new quantity must exceed the filled quantity"));
            }
            o.req.qty = q;
        }
        match (&mut o.req.kind, m.price, m.trigger) {
            (OrderKind::Limit { price }, Some(p), _) if p.0 > 0 => *price = p,
            (OrderKind::StopLossMarket { trigger }, _, Some(t)) if t.0 > 0 => *trigger = t,
            (_, None, None) => {}
            _ => return Err(api("PP400", "modify does not match the order type")),
        }
        self.persist(&g.acct)
    }

    async fn cancel(&self, id: &BrokerOrderId) -> BrokerResult<()> {
        let seq = seq_of(id)?;
        let mut g = self.lock();
        let o = g
            .acct
            .orders
            .get_mut(&seq)
            .ok_or_else(|| api("PP404", format!("unknown order {}", id.0)))?;
        if !o.working() {
            return Err(api("PP409", format!("order {} is already {}", id.0, o.status.as_str())));
        }
        o.status = OrderStatus::Cancelled;
        self.persist(&g.acct)
    }

    async fn positions(&self) -> BrokerResult<Vec<Position>> {
        let g = self.lock();
        Ok(g.acct
            .positions
            .iter()
            .map(|(tok, p)| Position {
                symbol: p.symbol.clone(),
                exchange: p.exchange.unwrap_or(Exchange::Nse),
                net_qty: p.qty,
                avg_price: if p.qty == 0 {
                    Price::ZERO
                } else {
                    Price(p.cost / p.qty.abs())
                },
                realized: p.realized,
                unrealized: p.unrealized(g.ltp.get(tok).copied()),
            })
            .collect())
    }

    async fn order_book(&self) -> BrokerResult<Vec<BrokerOrder>> {
        let g = self.lock();
        Ok(g.acct
            .orders
            .values()
            .map(|o| BrokerOrder {
                id: id_of(o.seq),
                client_order_id: Some(o.req.client_order_id.clone()),
                symbol: o.req.symbol.clone(),
                side: o.req.side,
                qty: o.req.qty,
                filled_qty: o.filled,
                avg_price: (o.filled > 0).then(|| Price(o.value / i64::from(o.filled))),
                status: o.status,
                reject_reason: o.reject_reason.clone(),
            })
            .collect())
    }

    async fn funds(&self) -> BrokerResult<Funds> {
        let g = self.lock();
        let eq = self.equity(&g);
        Ok(Funds {
            net: eq,
            available_cash: eq,
        })
    }

    async fn candles(&self, _inst: &Instrument, _tf: Tf, _from: Ts, _to: Ts) -> BrokerResult<Vec<Bar>> {
        Err(BrokerError::Unsupported(
            "paper trading has no history; read candles from data/",
        ))
    }
}
