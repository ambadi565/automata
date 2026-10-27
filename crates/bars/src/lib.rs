//! Bars and session statistics built from live ticks.
//!
//! - [`BarBuilder`] turns one instrument's ticks into 1-minute bars and rolls
//!   those up into higher timeframes (5-minute by default), anchored to the
//!   09:15 open the way Angel's candles are.
//! - [`OpeningRange`] and [`SessionVwap`] are the session-scoped levels the
//!   strategies use.
//! - [`SymbolState`] wires both to the `ta` indicators and exposes a
//!   [`Snapshot`] per symbol; [`BarHub`] holds one per subscribed token.
//!
//! Bar volume is the difference of the exchange's cumulative day volume, so
//! it needs Quote (or SnapQuote) mode. In LTP mode bars still form but carry
//! zero volume.

mod builder;
mod pipeline;
mod session;

pub use builder::{BarBuilder, BarConfig, BarEvent, PreOpenPolicy, TickOutcome, Trade};
pub use pipeline::{BarHub, IndicatorParams, Snapshot, SymbolState, Update};
pub use session::{OpeningRange, SessionVwap};
