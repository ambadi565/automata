use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("configuration: {0}")]
    Config(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("broker rejected request [{code}]: {message}")]
    Api { code: String, message: String },
    #[error("rate limited by broker: {0}")]
    RateLimited(String),
    #[error("transient broker/network error: {0}")]
    Transient(String),
    #[error("could not decode broker response: {0}")]
    Decode(String),
    #[error("not supported: {0}")]
    Unsupported(&'static str),
}

impl BrokerError {
    /// Worth retrying after a backoff.
    pub fn is_transient(&self) -> bool {
        matches!(self, BrokerError::RateLimited(_) | BrokerError::Transient(_))
    }
}

impl From<reqwest::Error> for BrokerError {
    fn from(e: reqwest::Error) -> Self {
        let msg = error_chain(&e);
        if e.is_decode() {
            BrokerError::Decode(msg)
        } else {
            BrokerError::Transient(msg)
        }
    }
}

/// `e` plus its sources, so "error sending request" says *why*.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

pub type BrokerResult<T> = Result<T, BrokerError>;
