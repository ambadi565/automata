//! `Backtest`: the [`Broker`] implementation for historical replay.
//!
//! [`SimExchange`] does the work synchronously (the backtest runner drives it
//! directly, bar by bar). [`Backtest`] wraps it behind the async [`Broker`]
//! trait so code written against the trait (the OMS from M3) runs unchanged
//! in a backtest.
//!
//! It never talks to a real broker: there is no network code here at all.

pub mod costs;
pub mod sim;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::Utc;
use saarthi_core::{Bar, Exchange, Instrument, Price, Tf, Ts};

pub use costs::{Brokerage, Charges, CostModel, ExecutionCosts, Slippage};
pub use sim::{FillKind, SimExchange, SimFill, SimPosition};

use crate::Broker;
use crate::error::{BrokerError, BrokerResult};
use crate::secret::Secret;
use crate::types::{BrokerOrder, BrokerOrderId, Funds, Modify, OrderRequest, Position, Session};

/// A replay broker: a [`SimExchange`] plus the candles it replays, so
/// [`Broker::candles`] answers from history.
#[derive(Clone)]
pub struct Backtest {
    sim: Arc<Mutex<SimExchange>>,
    candles: Arc<HashMap<(String, Tf), Vec<Bar>>>,
}

impl Backtest {
    pub fn new(sim: SimExchange) -> Self {
        Backtest {
            sim: Arc::new(Mutex::new(sim)),
            candles: Arc::new(HashMap::new()),
        }
    }

    /// Candles keyed by `(token, tf)`, oldest first.
    pub fn with_candles(mut self, candles: HashMap<(String, Tf), Vec<Bar>>) -> Self {
        self.candles = Arc::new(candles);
        self
    }

    /// Direct access to the simulated exchange (advance the clock, feed bars).
    pub fn sim(&self) -> MutexGuard<'_, SimExchange> {
        self.sim.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Feeds one bar and returns its fills.
    pub fn on_bar(&self, token: &str, bar: &Bar) -> Vec<SimFill> {
        self.sim().on_bar(token, bar)
    }

    pub fn set_time(&self, now: Ts) {
        self.sim().set_time(now);
    }
}

#[async_trait]
impl Broker for Backtest {
    async fn login(&self) -> BrokerResult<Session> {
        Ok(Session {
            client_code: "BACKTEST".into(),
            jwt: Secret::new(String::new()),
            refresh_token: Secret::new(String::new()),
            feed_token: Secret::new(String::new()),
            created_at: Utc::now(),
        })
    }

    async fn place(&self, req: OrderRequest) -> BrokerResult<BrokerOrderId> {
        self.sim().place(req)
    }

    async fn modify(&self, id: &BrokerOrderId, m: Modify) -> BrokerResult<()> {
        self.sim().modify(id, m)
    }

    async fn cancel(&self, id: &BrokerOrderId) -> BrokerResult<()> {
        self.sim().cancel(id)
    }

    async fn positions(&self) -> BrokerResult<Vec<Position>> {
        let sim = self.sim();
        let mut out: Vec<Position> = sim
            .positions()
            .map(|(token, p)| Position {
                symbol: saarthi_core::Symbol::new(token),
                exchange: Exchange::Nse,
                net_qty: p.qty,
                avg_price: p.avg_price().unwrap_or(Price::ZERO),
                realized: p.realized,
                unrealized: p.unrealized(),
            })
            .collect();
        out.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        Ok(out)
    }

    async fn order_book(&self) -> BrokerResult<Vec<BrokerOrder>> {
        Ok(self.sim().order_book())
    }

    async fn funds(&self) -> BrokerResult<Funds> {
        let eq = self.sim().equity();
        Ok(Funds {
            net: eq,
            available_cash: eq,
        })
    }

    async fn candles(&self, inst: &Instrument, tf: Tf, from: Ts, to: Ts) -> BrokerResult<Vec<Bar>> {
        let bars = self
            .candles
            .get(&(inst.token.clone(), tf))
            .ok_or(BrokerError::Unsupported(
                "no candles loaded for this instrument/timeframe",
            ))?;
        Ok(bars.iter().filter(|b| b.ts >= from && b.ts <= to).copied().collect())
    }
}
