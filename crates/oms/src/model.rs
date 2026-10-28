//! What the OMS persists: trades and the orders that make them up.

use chrono::NaiveDate;
use saarthi_broker::{BrokerOrderId, OrderRequest, OrderStatus};
use saarthi_core::{Exchange, Price, Qty, Side, Symbol, Ts};
use serde::{Deserialize, Serialize};

/// What an order is for within its trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Entry,
    /// The broker-side SL-M protecting the position.
    Stop,
    /// A market order closing the position (exit signal, target, square-off).
    Exit,
}

/// One order as the OMS knows it.
///
/// Lifecycle: `New` (persisted, not yet sent) → `Sent` (sent, broker has not
/// confirmed) → `Open` → `PartiallyFilled` → `Filled | Rejected | Cancelled`.
/// An order is written as `New` *before* it goes to the broker, so a crash
/// between the two is found and resolved on restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRec {
    pub client_id: String,
    pub broker_id: Option<BrokerOrderId>,
    pub trade_id: String,
    pub role: Role,
    pub req: OrderRequest,
    pub status: OrderStatus,
    pub filled: Qty,
    pub avg_price: Option<Price>,
    pub reject_reason: Option<String>,
    pub created: Ts,
    pub updated: Ts,
}

impl OrderRec {
    pub fn is_working(&self) -> bool {
        !self.status.is_terminal()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeState {
    /// Entry sent, nothing filled yet.
    Pending,
    /// Position on, protected by a broker-side stop.
    Open,
    /// Closing: cancel what is working, then exit at market until flat.
    Exiting,
    /// Flat; P&L final.
    Closed,
    /// The entry never filled.
    Cancelled,
}

impl TradeState {
    pub fn as_str(self) -> &'static str {
        match self {
            TradeState::Pending => "pending",
            TradeState::Open => "open",
            TradeState::Exiting => "exiting",
            TradeState::Closed => "closed",
            TradeState::Cancelled => "cancelled",
        }
    }

    pub fn is_done(self) -> bool {
        matches!(self, TradeState::Closed | TradeState::Cancelled)
    }

    pub const DONE: [&'static str; 2] = ["closed", "cancelled"];
}

/// What a strategy asked for, sized by the risk guard.
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    pub strategy: String,
    pub token: String,
    pub symbol: Symbol,
    pub exchange: Exchange,
    pub side: Side,
    pub qty: Qty,
    /// The signal's reference price, to measure slippage against.
    pub signal_px: Price,
    pub stop: Price,
    /// Managed by the engine (exit at market when crossed), not resting at
    /// the broker: Angel has no OCO for regular orders, and two resting exits
    /// could both fill.
    pub target: Option<Price>,
    pub planned_risk: Price,
    pub reason: String,
}

/// One round trip: entry, broker-side stop, exits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub id: String,
    pub day: NaiveDate,
    pub strategy: String,
    pub token: String,
    pub symbol: Symbol,
    pub exchange: Exchange,
    pub side: Side,
    pub planned_qty: Qty,
    pub signal_px: Price,
    pub signal_ts: Ts,
    pub stop: Price,
    pub target: Option<Price>,
    pub planned_risk: Price,
    pub reason: String,
    pub state: TradeState,
    pub entry_qty: Qty,
    /// Σ price × qty of entry fills.
    pub entry_value: i64,
    pub exit_qty: Qty,
    pub exit_value: i64,
    /// Estimated charges on every fill so far (cost model).
    pub charges: Price,
    pub exit_reason: Option<String>,
    pub opened_at: Option<Ts>,
    pub closed_at: Option<Ts>,
    /// Client ids of this trade's orders, in creation order.
    pub orders: Vec<String>,
    /// Market exit attempts that failed (rejected or lost).
    #[serde(default)]
    pub exit_failures: u32,
}

impl Trade {
    pub fn open_qty(&self) -> Qty {
        self.entry_qty.saturating_sub(self.exit_qty)
    }

    /// Signed open quantity: + long, − short.
    pub fn net_qty(&self) -> i64 {
        self.side.sign() * i64::from(self.open_qty())
    }

    pub fn avg_entry(&self) -> Option<Price> {
        (self.entry_qty > 0).then(|| Price(self.entry_value / i64::from(self.entry_qty)))
    }

    pub fn avg_exit(&self) -> Option<Price> {
        (self.exit_qty > 0).then(|| Price(self.exit_value / i64::from(self.exit_qty)))
    }

    /// P&L on the closed quantity, before charges.
    pub fn realized(&self) -> Price {
        let Some(avg) = self.avg_entry() else {
            return Price::ZERO;
        };
        let q = i64::from(self.exit_qty);
        let cost = avg.0 * q;
        Price(self.side.sign() * (self.exit_value - cost))
    }

    /// Mark-to-market of the open quantity at `ltp`.
    pub fn unrealized(&self, ltp: Option<Price>) -> Price {
        match (self.avg_entry(), ltp) {
            (Some(avg), Some(ltp)) => Price(self.side.sign() * (ltp.0 - avg.0) * i64::from(self.open_qty())),
            _ => Price::ZERO,
        }
    }

    /// Realized + unrealized − charges.
    pub fn net(&self, ltp: Option<Price>) -> Price {
        self.realized() + self.unrealized(ltp) - self.charges
    }

    /// Exposure for the risk guard: open quantity at `ltp`, or the planned
    /// quantity at the signal price while the entry works.
    pub fn exposure(&self, ltp: Option<Price>) -> Price {
        match self.state {
            TradeState::Pending => self.signal_px * self.planned_qty,
            TradeState::Open | TradeState::Exiting => {
                ltp.or(self.avg_entry()).unwrap_or(self.signal_px) * self.open_qty()
            }
            TradeState::Closed | TradeState::Cancelled => Price::ZERO,
        }
    }
}
