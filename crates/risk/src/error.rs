use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiskError {
    #[error("risk config: {0}")]
    Config(String),
}

pub type RiskResult<T> = Result<T, RiskError>;
