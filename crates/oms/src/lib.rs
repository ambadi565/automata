//! Order management (ARCHITECTURE §5.8).
//!
//! * Order state machine: `New → Sent → Open → PartiallyFilled → Filled |
//!   Rejected | Cancelled`, every transition persisted to SQLite, every
//!   order written *before* it is sent.
//! * Every entry fill is paired at once with a broker-side SL-M for the
//!   filled quantity, resized as partial fills arrive, so a position stays
//!   protected if the engine or its connection dies. Targets are managed by
//!   the engine (one resting exit order per position, never two).
//! * Exits cancel the entry remainder and the stop first, then send a market
//!   order for what is open, and retry until flat.
//! * Reconciliation trusts the broker: unknown positions are exited and the
//!   engine halts; positions closed behind our back are written off.
//! * Crash recovery: [`Oms::new`] reloads unfinished trades, [`Oms::recover`]
//!   catches up with the broker's book and re-arms missing stops.
//!
//! The OMS never decides *whether* to trade; the risk guard does.

pub mod error;
pub mod model;
mod oms;

pub use error::{OmsError, OmsResult};
pub use model::{Intent, OrderRec, Role, Trade, TradeState};
pub use oms::{Oms, OmsConfig, OmsEvent, Reconciliation, Severity};
