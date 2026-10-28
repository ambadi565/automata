//! `backtest.toml` plus the parts of `risk.toml` a backtest enforces.

use std::path::Path;

use anyhow::{Context, Result, bail};
use saarthi_core::Tf;
use serde::{Deserialize, Serialize};

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BacktestConfig {
    pub capital_inr: f64,
    pub warmup_days: usize,
    pub tf: Tf,
    pub walk_forward: WalkForwardConfig,
    pub gate: GateConfig,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalkForwardConfig {
    pub train_days: usize,
    pub test_days: usize,
    pub min_train_trades: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    pub min_profit_factor: f64,
    pub max_drawdown_pct: f64,
    pub min_trades: usize,
    pub min_neighbour_profitable_share: f64,
}

impl BacktestConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let c: BacktestConfig = toml::from_str(&read(path)?).with_context(|| format!("parsing {}", path.display()))?;
        if c.walk_forward.train_days == 0 || c.walk_forward.test_days == 0 {
            bail!("walk_forward train_days and test_days must be positive");
        }
        if !c.tf.is_intraday() {
            bail!("backtests run on intraday bars, not {}", c.tf);
        }
        Ok(c)
    }
}

/// The `risk.toml` limits, shared with the live risk guard. The backtester
/// applies the subset that changes results (sizing, positions, exposure,
/// daily loss, trades per day, cooldown, cut-offs).
pub use saarthi_risk::RiskLimits;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config").join(name)
    }

    #[test]
    fn shipped_configs_parse() {
        let b = BacktestConfig::load(&cfg("backtest.toml")).unwrap();
        assert_eq!(b.tf, Tf::M5);
        assert!((b.gate.min_profit_factor - 1.3).abs() < 1e-9);
        assert!((b.gate.max_drawdown_pct - 8.0).abs() < 1e-9);
        assert_eq!(b.gate.min_trades, 200);
        let r = RiskLimits::load(&cfg("risk.toml")).unwrap();
        assert!(r.max_risk_per_trade_pct > 0.0);
        r.session().unwrap();
    }
}
