//! `engine.toml`, the parts `fetch` needs. Unknown sections are ignored so
//! the same file serves the engine in later milestones.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use saarthi_broker::smartapi::SmartApiConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct EngineConfig {
    pub paths: Paths,
    pub smartapi: SmartApiConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub universe: PathBuf,
    pub holidays: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            data_dir: "data".into(),
            universe: "config/universe.toml".into(),
            holidays: "config/holidays.toml".into(),
        }
    }
}

impl EngineConfig {
    /// Loads `path`; a missing file means all defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "engine config not found; using defaults");
                Ok(Self::default())
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_engine_toml_parses() {
        let p = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/engine.toml"));
        let c = EngineConfig::load(p).unwrap();
        assert!(
            c.smartapi.historical_requests_per_sec <= 3.0,
            "must stay under the documented 3 req/s"
        );
        assert!(c.smartapi.base_url.starts_with("https://"));
    }

    #[test]
    fn shipped_risk_toml_has_every_hard_limit() {
        let p = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/risk.toml"));
        let v: toml::Table = toml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
        for key in [
            "capital_base_inr",
            "max_risk_per_trade_pct",
            "max_open_positions",
            "max_gross_exposure_x",
            "max_daily_loss_pct",
            "daily_profit_lock_pct",
            "max_trades_per_day",
            "max_consecutive_losses",
            "min_price_inr",
            "no_new_entries_after",
            "square_off_at",
            "max_order_qty",
            "max_orders_per_second",
        ] {
            assert!(v.contains_key(key), "risk.toml is missing {key}");
        }
    }
}
