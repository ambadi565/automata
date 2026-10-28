//! Storage for Saarthi.
//!
//! * [`parquet`]: bars and the instrument master as Parquet. Prices are stored
//!   as `Int64` paise, timestamps as UTC milliseconds tagged `Asia/Kolkata`.
//! * [`manifest`]: which date ranges the fetcher has already covered, so
//!   interrupted downloads resume instead of starting over.
//!
//! Every write goes to a temp file and is renamed into place, so a crash or
//! `kill -9` never leaves a half-written file behind.

pub mod error;
pub mod layout;
pub mod manifest;
pub mod parquet;

pub use error::{StoreError, StoreResult};
pub use layout::DataLayout;
pub use manifest::{Coverage, Manifest};

use std::io::Write;
use std::path::Path;

/// Writes `bytes` to `path` atomically (temp file in the same dir + rename).
pub fn atomic_write(path: &Path, write: impl FnOnce(&mut std::fs::File) -> StoreResult<()>) -> StoreResult<()> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError::Io(format!("{} has no parent", path.display())))?;
    std::fs::create_dir_all(dir).map_err(|e| StoreError::io(dir, e))?;
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = dir.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp).map_err(|e| StoreError::io(&tmp, e))?;
        write(&mut f)?;
        f.flush().map_err(|e| StoreError::io(&tmp, e))?;
        f.sync_all().map_err(|e| StoreError::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| StoreError::io(path, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}
