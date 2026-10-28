use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use saarthi_core::{Symbol, Tf};

/// Where things live under the data directory.
///
/// Market data is shared by every account on a host; per-account state
/// (SQLite, journals, reports) goes under `accounts/<client_code>/`.
#[derive(Debug, Clone)]
pub struct DataLayout {
    root: PathBuf,
}

impl DataLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DataLayout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn instruments(&self) -> PathBuf {
        self.root.join("instruments.parquet")
    }

    pub fn candles_dir(&self, tf: Tf) -> PathBuf {
        self.root.join("candles").join(tf.as_str())
    }

    pub fn candles(&self, tf: Tf, sym: &Symbol) -> PathBuf {
        self.candles_dir(tf).join(format!("{}.parquet", sym.file_stem()))
    }

    pub fn manifest(&self, tf: Tf) -> PathBuf {
        self.candles_dir(tf).join("_manifest.json")
    }

    /// One recorded live session: raw feed frames plus the bars built from
    /// them, kept apart from the REST candles they are checked against.
    pub fn live_dir(&self, date: NaiveDate) -> PathBuf {
        self.root.join("live").join(date.to_string())
    }

    pub fn live_frames(&self, date: NaiveDate) -> PathBuf {
        self.live_dir(date).join("frames.bin")
    }

    pub fn live_bars(&self, date: NaiveDate, tf: Tf, sym: &Symbol) -> PathBuf {
        self.live_dir(date)
            .join(tf.as_str())
            .join(format!("{}.parquet", sym.file_stem()))
    }

    pub fn account_dir(&self, client_code: &str) -> PathBuf {
        let safe: String = client_code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        self.root.join("accounts").join(safe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let l = DataLayout::new("/d");
        assert_eq!(
            l.candles(Tf::M5, &Symbol::new("M&M-EQ")),
            PathBuf::from("/d/candles/5m/M&M-EQ.parquet")
        );
        assert_eq!(l.manifest(Tf::D1), PathBuf::from("/d/candles/1d/_manifest.json"));
        assert_eq!(l.account_dir("../A1"), PathBuf::from("/d/accounts/A1"));
        let d = "2026-10-27".parse().unwrap();
        assert_eq!(l.live_frames(d), PathBuf::from("/d/live/2026-10-27/frames.bin"));
        assert_eq!(
            l.live_bars(d, Tf::M1, &Symbol::new("TCS-EQ")),
            PathBuf::from("/d/live/2026-10-27/1m/TCS-EQ.parquet")
        );
    }
}
