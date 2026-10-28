//! `engine.toml` as the engine reads it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fetch::config::Paths;
use saarthi_bars::{BarConfig, IndicatorParams};
use saarthi_broker::smartapi::SmartApiConfig;
use saarthi_feed::FeedConfig;
use saarthi_strategy::{Direction, registry};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Backtest,
    Paper,
    Live,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub mode: Option<Mode>,
    pub paths: Paths,
    pub smartapi: SmartApiConfig,
    pub feed: FeedConfig,
    pub bars: BarConfig,
    pub engine: EngineSettings,
}

/// The `[engine]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineSettings {
    pub risk: PathBuf,
    pub costs: PathBuf,
    /// Strategies that may trade. Only add one after it passes the M2 gate on
    /// real data (paper trading is where you watch it first).
    pub strategies: Vec<String>,
    /// Subset of the universe to trade (empty = all of it).
    pub symbols: Vec<String>,
    pub direction: Direction,
    pub indicators: IndicatorParams,
    /// Paper account starting cash, rupees (used only for a new account).
    pub paper_capital_inr: f64,
    /// Days of 5m candles from `data/` to warm indicators up with.
    pub warmup_days: usize,
    /// Broker order book poll interval (live; paper syncs on every fill).
    pub sync_secs: u64,
    /// Compare positions with the broker this often.
    pub reconcile_secs: u64,
    /// While squaring off, re-check flatness this often.
    pub square_off_retry_secs: u64,
    /// Alert (critical) if still not flat after this many checks.
    pub square_off_alert_after: u32,
    /// Stop the daemon at this IST time on a trading day.
    pub stop_at: String,
    /// Presence of this file (in the account dir) is the kill switch.
    pub halt_file: String,
    /// The conservative plan used when the strategist has not written one.
    pub default_plan: PathBuf,
    /// Control API address (loopback only). Empty disables it.
    pub control_addr: String,
    /// Indices shown on `/context`: name → NSE token.
    pub index_tokens: Vec<(String, String)>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        EngineSettings {
            risk: "config/risk.toml".into(),
            costs: "config/costs.toml".into(),
            strategies: registry::IDS.iter().map(|s| s.to_string()).collect(),
            symbols: Vec::new(),
            direction: Direction::Both,
            indicators: IndicatorParams::default(),
            paper_capital_inr: 1_000_000.0,
            warmup_days: 5,
            sync_secs: 1,
            reconcile_secs: 30,
            square_off_retry_secs: 5,
            square_off_alert_after: 3,
            stop_at: "15:35".into(),
            halt_file: "HALT".into(),
            default_plan: "config/plan.default.json".into(),
            control_addr: "127.0.0.1:7070".into(),
            index_tokens: vec![
                ("NIFTY".into(), "99926000".into()),
                ("BANKNIFTY".into(), "99926009".into()),
                ("INDIA VIX".into(), "99926017".into()),
            ],
        }
    }
}

impl Config {
    /// Loads `path`; a missing file means all defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let c: Config = match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "engine config not found; using defaults");
                Config::default()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        for s in &self.engine.strategies {
            registry::specs(s).with_context(|| format!("[engine] strategies: {s}"))?;
        }
        let tf = self.engine.indicators.tf;
        if tf != saarthi_core::Tf::M1 && !self.bars.higher.contains(&tf) {
            bail!("[engine.indicators] tf = {tf} but [bars] higher does not build it");
        }
        if self.engine.paper_capital_inr <= 0.0 {
            bail!("[engine] paper_capital_inr must be positive");
        }
        saarthi_core::clock::parse_hhmm(&self.engine.stop_at)?;
        if !self.engine.control_addr.is_empty() {
            let a: std::net::SocketAddr = self
                .engine
                .control_addr
                .parse()
                .with_context(|| format!("[engine] control_addr = {:?}", self.engine.control_addr))?;
            if !a.ip().is_loopback() {
                bail!("[engine] control_addr must be a loopback address (it has no TLS)");
            }
        }
        Ok(())
    }

    /// `SAARTHI_MODE` overrides the file.
    pub fn mode(&self) -> Result<Mode> {
        match std::env::var("SAARTHI_MODE") {
            Ok(m) => match m.to_ascii_lowercase().as_str() {
                "paper" => Ok(Mode::Paper),
                "live" => Ok(Mode::Live),
                "backtest" => Ok(Mode::Backtest),
                other => bail!("SAARTHI_MODE={other:?}: expected paper, live or backtest"),
            },
            Err(_) => Ok(self.mode.unwrap_or(Mode::Paper)),
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
        assert_eq!(c.mode, Some(Mode::Paper));
        assert!(!c.engine.strategies.is_empty());
        assert_eq!(c.engine.reconcile_secs, 30);
    }

    #[test]
    fn unknown_strategy_is_rejected() {
        let c: Config = toml::from_str("[engine]\nstrategies = [\"martingale\"]\n").unwrap();
        assert!(c.validate().is_err());
    }
}
