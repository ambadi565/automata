//! Hard risk limits (`risk.toml`) and the guard that enforces them.
//!
//! The guard is sovereign: strategies propose, the plan (M4) scales, but
//! nothing reaches the broker without passing [`RiskGuard::check`], and the
//! daily P&L monitor can flatten and halt regardless of what anyone wants.

pub mod error;
pub mod guard;
pub mod limits;
pub mod throttle;

pub use error::{RiskError, RiskResult};
pub use guard::{Approval, Book, Breach, EntryProposal, HaltReason, PriceBand, Rejection, RiskGuard, RiskState};
pub use limits::{RiskLimits, inr, pct_of};
pub use throttle::OrderThrottle;
