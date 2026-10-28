//! Order and account types shared by all broker implementations.

use chrono::{DateTime, Utc};
use saarthi_core::{Exchange, Price, Qty, Side, Symbol};

use crate::secret::Secret;

/// An authenticated broker session. Tokens are secrets.
#[derive(Debug, Clone)]
pub struct Session {
    pub client_code: String,
    pub jwt: Secret,
    pub refresh_token: Secret,
    pub feed_token: Secret,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrokerOrderId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    Intraday,
    Delivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    Market,
    Limit { price: Price },
    StopLossMarket { trigger: Price },
    StopLossLimit { trigger: Price, price: Price },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    /// Our idempotency key; mapped to the broker id by the OMS.
    pub client_order_id: String,
    pub symbol: Symbol,
    pub exchange: Exchange,
    pub token: String,
    pub side: Side,
    pub qty: Qty,
    pub kind: OrderKind,
    pub product: Product,
    /// Order tag for audit and compliance.
    pub tag: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Modify {
    pub qty: Option<Qty>,
    pub price: Option<Price>,
    pub trigger: Option<Price>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    New,
    Sent,
    Open,
    PartiallyFilled,
    Filled,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerOrder {
    pub id: BrokerOrderId,
    pub client_order_id: Option<String>,
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Qty,
    pub filled_qty: Qty,
    pub avg_price: Option<Price>,
    pub status: OrderStatus,
    pub reject_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub symbol: Symbol,
    pub exchange: Exchange,
    /// Positive = long, negative = short.
    pub net_qty: i64,
    pub avg_price: Price,
    pub realized: Price,
    pub unrealized: Price,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Funds {
    pub net: Price,
    pub available_cash: Price,
}
