#![allow(dead_code, clippy::inconsistent_digit_grouping)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{NaiveDate, NaiveTime, TimeDelta};
use saarthi_broker::backtest::{CostModel, ExecutionCosts, Slippage};
use saarthi_broker::{
    Broker, BrokerError, BrokerOrder, BrokerOrderId, BrokerResult, Funds, Modify, OrderRequest, Paper, PaperConfig,
    Position, Session,
};
use saarthi_core::{Bar, Exchange, Instrument, MarketClock, Price, Side, Symbol, Tf, Tick, Ts};
use saarthi_oms::{Intent, Oms, OmsConfig};
use saarthi_risk::OrderThrottle;
use saarthi_store::Db;

pub fn costs() -> CostModel {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/costs.toml");
    ExecutionCosts::load(Path::new(p)).unwrap().charges
}

pub fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
}

pub fn t0() -> Ts {
    MarketClock::at(day(), NaiveTime::from_hms_opt(9, 30, 0).unwrap())
}

pub fn paper_cfg(state: Option<std::path::PathBuf>, partial: Option<u32>) -> PaperConfig {
    PaperConfig {
        capital: Price(10_00_000_00),
        costs: costs(),
        slippage: Slippage {
            bps: 0.0,
            ticks: 0,
            tick_size_inr: 0.05,
            limit_through_ticks: 1,
        },
        max_fill_qty_per_tick: partial,
        state_path: state,
    }
}

/// What the next call of a kind should do instead of reaching the paper
/// broker.
#[derive(Debug, Clone)]
pub enum Fault {
    /// Rejected by the broker.
    Reject,
    /// The request never arrives (network error before sending).
    Drop,
    /// The broker takes the order but the reply is lost.
    LoseReply,
    /// The call never returns (the process dies while waiting).
    Hang,
}

/// The paper broker with injectable failures.
pub struct Faulty {
    pub paper: Paper,
    pub place_faults: Mutex<VecDeque<Fault>>,
    pub placed: Mutex<Vec<OrderRequest>>,
}

impl Faulty {
    pub fn new(paper: Paper) -> Self {
        Faulty {
            paper,
            place_faults: Mutex::new(VecDeque::new()),
            placed: Mutex::new(Vec::new()),
        }
    }

    pub fn fail_next_place(&self, f: Fault) {
        self.place_faults.lock().unwrap().push_back(f);
    }
}

#[async_trait]
impl Broker for Faulty {
    async fn login(&self) -> BrokerResult<Session> {
        self.paper.login().await
    }
    async fn place(&self, req: OrderRequest) -> BrokerResult<BrokerOrderId> {
        let fault = self.place_faults.lock().unwrap().pop_front();
        match fault {
            Some(Fault::Reject) => Err(BrokerError::Api {
                code: "AB4008".into(),
                message: "insufficient margin".into(),
            }),
            Some(Fault::Drop) => Err(BrokerError::Transient("connection reset".into())),
            Some(Fault::Hang) => std::future::pending().await,
            Some(Fault::LoseReply) => {
                self.placed.lock().unwrap().push(req.clone());
                self.paper.place(req).await?;
                Err(BrokerError::Transient("timed out waiting for reply".into()))
            }
            None => {
                self.placed.lock().unwrap().push(req.clone());
                self.paper.place(req).await
            }
        }
    }
    async fn modify(&self, id: &BrokerOrderId, m: Modify) -> BrokerResult<()> {
        self.paper.modify(id, m).await
    }
    async fn cancel(&self, id: &BrokerOrderId) -> BrokerResult<()> {
        self.paper.cancel(id).await
    }
    async fn positions(&self) -> BrokerResult<Vec<Position>> {
        self.paper.positions().await
    }
    async fn order_book(&self) -> BrokerResult<Vec<BrokerOrder>> {
        self.paper.order_book().await
    }
    async fn funds(&self) -> BrokerResult<Funds> {
        self.paper.funds().await
    }
    async fn candles(&self, i: &Instrument, tf: Tf, from: Ts, to: Ts) -> BrokerResult<Vec<Bar>> {
        self.paper.candles(i, tf, from, to).await
    }
}

pub struct Rig {
    pub broker: Arc<Faulty>,
    pub oms: Oms<Faulty>,
    pub now: Ts,
}

pub fn oms_cfg() -> OmsConfig {
    OmsConfig::new(costs())
}

impl Rig {
    pub fn new(partial: Option<u32>) -> Rig {
        let paper = Paper::open(paper_cfg(None, partial)).unwrap();
        let broker = Arc::new(Faulty::new(paper));
        let oms = Oms::new(
            broker.clone(),
            Db::in_memory().unwrap(),
            OrderThrottle::new(50),
            oms_cfg(),
            day(),
        )
        .unwrap();
        Rig { broker, oms, now: t0() }
    }

    /// One tick for `token` at `ltp` (paise), then an OMS sync.
    pub async fn tick(&mut self, token: &str, ltp: i64) {
        self.now += TimeDelta::seconds(1);
        self.broker.paper.on_tick(&tick(token, self.now, ltp)).unwrap();
        self.oms.sync(self.now).await.unwrap();
    }
}

pub fn tick(token: &str, ts: Ts, ltp: i64) -> Tick {
    Tick {
        token: token.into(),
        exchange: Exchange::Nse,
        ts,
        ltp: Price(ltp),
        last_qty: Some(100),
        day_volume: None,
        avg_price: None,
    }
}

pub fn symbol_of(token: &str) -> Symbol {
    Symbol::new(format!("SYM{token}-EQ"))
}

/// Long `qty` at ₹1000, stop ₹990, target ₹1020.
pub fn long(token: &str, qty: u32) -> Intent {
    Intent {
        strategy: "orb".into(),
        token: token.into(),
        symbol: symbol_of(token),
        exchange: Exchange::Nse,
        side: Side::Buy,
        qty,
        signal_px: Price(1000_00),
        stop: Price(990_00),
        target: Some(Price(1020_00)),
        planned_risk: Price(10_00 * i64::from(qty)),
        reason: "test".into(),
    }
}

pub fn short(token: &str, qty: u32) -> Intent {
    Intent {
        side: Side::Sell,
        stop: Price(1010_00),
        target: Some(Price(980_00)),
        ..long(token, qty)
    }
}
