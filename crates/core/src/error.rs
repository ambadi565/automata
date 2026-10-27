use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid price {0:?}")]
    InvalidPrice(String),
    #[error("unknown timeframe {0:?}")]
    UnknownTimeframe(String),
    #[error("unknown exchange {0:?}")]
    UnknownExchange(String),
    #[error("invalid time {0:?} (expected HH:MM)")]
    InvalidTime(String),
    #[error("holiday calendar: {0}")]
    Calendar(String),
}

pub type CoreResult<T> = Result<T, CoreError>;
