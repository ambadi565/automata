use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::error::{StoreError, StoreResult};

/// Inclusive date range already requested from the broker for one symbol.
///
/// Tracked separately from the bars on disk so that ranges where the broker
/// has no data (before a listing date, holidays) are not re-requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl Coverage {
    /// Ranges to request so that coverage becomes `[from, to]` ∪ existing.
    ///
    /// Coverage stays one contiguous range: a request that lies beyond the
    /// existing range also fills the gap in between. The last covered day is
    /// always refetched because it may have been a partial session.
    pub fn missing(existing: Option<Coverage>, from: NaiveDate, to: NaiveDate) -> Vec<(NaiveDate, NaiveDate)> {
        if from > to {
            return vec![];
        }
        let Some(c) = existing else { return vec![(from, to)] };
        let mut out = Vec::new();
        if from < c.from
            && let Some(head_end) = c.from.pred_opt()
        {
            out.push((from, head_end));
        }
        if to >= c.to {
            out.push((c.to, to));
        }
        out
    }

    pub fn extend(existing: Option<Coverage>, from: NaiveDate, to: NaiveDate) -> Coverage {
        match existing {
            None => Coverage { from, to },
            Some(c) => Coverage {
                from: c.from.min(from),
                to: c.to.max(to),
            },
        }
    }
}

/// Per-timeframe record of what the fetcher has covered, keyed by symbol.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(skip)]
    path: PathBuf,
    pub symbols: BTreeMap<String, Coverage>,
}

impl Manifest {
    pub fn load(path: &Path) -> StoreResult<Self> {
        let mut m = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<Manifest>(&bytes)
                .map_err(|e| StoreError::Manifest(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest::default(),
            Err(e) => return Err(StoreError::io(path, e)),
        };
        m.path = path.to_path_buf();
        Ok(m)
    }

    pub fn get(&self, symbol: &str) -> Option<Coverage> {
        self.symbols.get(symbol).copied()
    }

    pub fn record(&mut self, symbol: &str, from: NaiveDate, to: NaiveDate) -> StoreResult<()> {
        let c = Coverage::extend(self.get(symbol), from, to);
        self.symbols.insert(symbol.to_string(), c);
        self.save()
    }

    pub fn save(&self) -> StoreResult<()> {
        let json = serde_json::to_vec_pretty(self).map_err(|e| StoreError::Manifest(e.to_string()))?;
        crate::atomic_write(&self.path, |f| {
            use std::io::Write;
            f.write_all(&json).map_err(|e| StoreError::io(&self.path, e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn cov(a: &str, b: &str) -> Option<Coverage> {
        Some(Coverage { from: d(a), to: d(b) })
    }

    #[test]
    fn missing_ranges() {
        assert_eq!(
            Coverage::missing(None, d("2026-01-01"), d("2026-02-01")),
            vec![(d("2026-01-01"), d("2026-02-01"))]
        );
        // Fully covered except the last day, which is refetched.
        assert_eq!(
            Coverage::missing(cov("2026-01-01", "2026-02-01"), d("2026-01-01"), d("2026-02-01")),
            vec![(d("2026-02-01"), d("2026-02-01"))]
        );
        // Extending forward.
        assert_eq!(
            Coverage::missing(cov("2026-01-01", "2026-02-01"), d("2026-01-01"), d("2026-03-01")),
            vec![(d("2026-02-01"), d("2026-03-01"))]
        );
        // Extending backward and forward.
        assert_eq!(
            Coverage::missing(cov("2026-01-10", "2026-02-01"), d("2026-01-01"), d("2026-02-05")),
            vec![(d("2026-01-01"), d("2026-01-09")), (d("2026-02-01"), d("2026-02-05"))]
        );
        // Request entirely after coverage: the gap is filled too.
        assert_eq!(
            Coverage::missing(cov("2025-01-01", "2025-02-01"), d("2026-01-01"), d("2026-01-05")),
            vec![(d("2025-02-01"), d("2026-01-05"))]
        );
        // Request entirely before coverage: head up to the covered range.
        assert_eq!(
            Coverage::missing(cov("2026-03-01", "2026-04-01"), d("2026-01-01"), d("2026-01-05")),
            vec![(d("2026-01-01"), d("2026-02-28"))]
        );
        // Request inside coverage that ends before its last day: nothing.
        assert!(Coverage::missing(cov("2026-01-01", "2026-04-01"), d("2026-02-01"), d("2026-03-01")).is_empty());
        assert!(Coverage::missing(None, d("2026-01-02"), d("2026-01-01")).is_empty());
    }

    #[test]
    fn manifest_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.json");
        let mut m = Manifest::load(&path).unwrap();
        assert!(m.get("A").is_none());
        m.record("A", d("2026-01-05"), d("2026-01-10")).unwrap();
        m.record("A", d("2026-01-01"), d("2026-01-06")).unwrap();
        let m2 = Manifest::load(&path).unwrap();
        assert_eq!(m2.get("A"), cov("2026-01-01", "2026-01-10"));
    }
}
