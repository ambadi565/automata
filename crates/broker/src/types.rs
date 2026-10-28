//! Order and account types shared by all broker implementations.

use chrono::{DateTime, Utc};
use saarthi_core::{Exchange, Price, Qty, Side, Symbol};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrokerOrderId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    Intraday,
    Delivery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrderKind {
    Market,
    Limit { price: Price },
    StopLossMarket { trigger: Price },
    StopLossLimit { trigger: Price, price: Price },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modify {
    pub qty: Option<Qty>,
    pub price: Option<Price>,
    pub trigger: Option<Price>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    New,
    Sent,
    Open,
    PartiallyFilled,
    Filled,
    Rejected,
    Cancelled,
}

impl OrderStatus {
    /// Filled, rejected or cancelled: nothing more can happen to it.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Rejected | OrderStatus::Cancelled
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::New => "new",
            OrderStatus::Sent => "sent",
            OrderStatus::Open => "open",
            OrderStatus::PartiallyFilled => "partially_filled",
            OrderStatus::Filled => "filled",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub exchange: Exchange,
    /// Positive = long, negative = short.
    pub net_qty: i64,
    pub avg_price: Price,
    pub realized: Price,
    pub unrealized: Price,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Funds {
    pub net: Price,
    pub available_cash: Price,
}
