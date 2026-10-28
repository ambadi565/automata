//! Broker abstraction plus the Angel One SmartAPI adapter.
//!
//! Strategies and the OMS only ever see the [`Broker`] trait, so the same code
//! runs against backtest, paper and live implementations. [`Backtest`]
//! replays history through a simulated exchange with Indian intraday charges
//! and slippage; it has no network code.

pub mod backtest;
pub mod error;
pub mod ratelimit;
pub mod secret;
pub mod smartapi;
pub mod types;

use async_trait::async_trait;
use saarthi_core::{Bar, Instrument, Tf, Ts};

pub use backtest::Backtest;
pub use error::{BrokerError, BrokerResult};
pub use ratelimit::RateLimiter;
pub use secret::Secret;
pub use types::*;

#[async_trait]
pub trait Broker: Send + Sync {
    async fn login(&self) -> BrokerResult<Session>;
    async fn place(&self, req: OrderRequest) -> BrokerResult<BrokerOrderId>;
    async fn modify(&self, id: &BrokerOrderId, m: Modify) -> BrokerResult<()>;
    async fn cancel(&self, id: &BrokerOrderId) -> BrokerResult<()>;
    async fn positions(&self) -> BrokerResult<Vec<Position>>;
    async fn order_book(&self) -> BrokerResult<Vec<BrokerOrder>>;
    async fn funds(&self) -> BrokerResult<Funds>;
    /// Historical bars for `[from, to]`. Takes the full [`Instrument`] (not
    /// just the symbol) because brokers address history by exchange token.
    async fn candles(&self, inst: &Instrument, tf: Tf, from: Ts, to: Ts) -> BrokerResult<Vec<Bar>>;
}
