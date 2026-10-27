//! The parts of `engine.toml` that `feedcheck` reads.

use std::path::Path;

use anyhow::{Context, Result};
use fetch::config::Paths;
use saarthi_bars::BarConfig;
use saarthi_broker::smartapi::SmartApiConfig;
use saarthi_feed::FeedConfig;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub paths: Paths,
    pub smartapi: SmartApiConfig,
    pub feed: FeedConfig,
    pub bars: BarConfig,
}

impl Config {
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
        let c = Config::load(p).unwrap();
        assert!(c.feed.url.starts_with("wss://"));
        assert!(c.feed.heartbeat_ms < 30_000, "Angel drops connections silent for 30 s");
        assert!(c.feed.stale_warn_ms <= c.feed.stale_critical_ms);
        assert_eq!(c.bars.higher, vec![saarthi_core::Tf::M5]);
    }
}
