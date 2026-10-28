//! Core domain types shared by every Saarthi crate.
//!
//! Money is always integer paise ([`Price`]), never `f64`. Floating point is
//! accepted only at the broker boundary and converted immediately.

pub mod clock;
pub mod error;
pub mod money;
pub mod types;

pub use clock::{Holiday, HolidayCalendar, IST, MarketClock, Phase, SessionTimes};
pub use error::{CoreError, CoreResult};
pub use money::{Price, Qty};
pub use types::{Bar, Exchange, Instrument, Side, Symbol, Tf, Tick, Ts};
