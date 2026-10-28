use saarthi_broker::BrokerError;
use saarthi_store::StoreError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OmsError {
    #[error("broker: {0}")]
    Broker(#[from] BrokerError),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("unknown trade {0}")]
    UnknownTrade(String),
    #[error("invalid order: {0}")]
    Invalid(String),
}

pub type OmsResult<T> = Result<T, OmsError>;
