use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("parquet: {0}")]
    Parquet(String),
    #[error("schema mismatch in {path}: {detail}")]
    Schema { path: String, detail: String },
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("stored record does not decode: {0}")]
    Decode(String),
}

impl StoreError {
    pub fn io(path: &Path, e: std::io::Error) -> Self {
        StoreError::Io(format!("{}: {e}", path.display()))
    }
}

impl From<parquet::errors::ParquetError> for StoreError {
    fn from(e: parquet::errors::ParquetError) -> Self {
        StoreError::Parquet(e.to_string())
    }
}

impl From<arrow_schema::ArrowError> for StoreError {
    fn from(e: arrow_schema::ArrowError) -> Self {
        StoreError::Parquet(e.to_string())
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::Decode(e.to_string())
    }
}
