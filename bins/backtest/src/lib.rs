//! The `backtest` binary's library: replay historical bars through the
//! strategies on a simulated exchange, optimise walk-forward, and judge the
//! out-of-sample results against the M2 go-live gate.
//!
//! Nothing here can reach a broker: execution is [`saarthi_broker::backtest`].

pub mod config;
pub mod data;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod synth;
pub mod walkforward;

use std::path::Path;

use anyhow::{Context, Result, bail};
use saarthi_core::Symbol;

/// Marker left next to synthetic candles so every report built on them says
/// so.
pub const SYNTHETIC_MARKER: &str = "_SYNTHETIC";

pub const SYNTHETIC_NOTE: &str = "SYNTHETIC DATA. These bars were generated, not recorded. The results test the \
     plumbing only and say nothing about how a strategy would do in the market. Rerun on real candles from \
     `fetch` before reading anything into the gate.";

/// Symbols from a universe file (`symbols = [...]`, as `config/universe.toml`).
pub fn load_universe(path: &Path) -> Result<Vec<Symbol>> {
    #[derive(serde::Deserialize)]
    struct U {
        symbols: Vec<String>,
    }
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let u: U = toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
    if u.symbols.is_empty() {
        bail!("{} lists no symbols", path.display());
    }
    Ok(u.symbols.into_iter().map(Symbol::new).collect())
}
