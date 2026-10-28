//! Historical bars for the universe, merged into one timeline.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use saarthi_core::{Bar, MarketClock, Symbol, Tf, Ts};
use saarthi_store::DataLayout;

#[derive(Debug, Clone)]
pub struct SymbolBars {
    pub symbol: Symbol,
    pub bars: Vec<Bar>,
}

/// Every symbol's bars plus a time-ordered index over all of them.
#[derive(Debug, Clone)]
pub struct Dataset {
    pub tf: Tf,
    pub symbols: Vec<SymbolBars>,
    /// `(bar start, symbol index, bar index)`, sorted by time then symbol.
    pub events: Vec<(Ts, u32, u32)>,
    /// Distinct IST trading days present, ascending.
    pub days: Vec<NaiveDate>,
}

impl Dataset {
    pub fn new(tf: Tf, mut symbols: Vec<SymbolBars>) -> Self {
        symbols.retain(|s| !s.bars.is_empty());
        symbols.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        let mut events = Vec::new();
        for (si, s) in symbols.iter_mut().enumerate() {
            s.bars.sort_by_key(|b| b.ts);
            s.bars.dedup_by_key(|b| b.ts);
            s.bars.retain(Bar::is_consistent);
            events.extend(s.bars.iter().enumerate().map(|(bi, b)| (b.ts, si as u32, bi as u32)));
        }
        events.sort_unstable();
        let mut days: Vec<NaiveDate> = events.iter().map(|e| MarketClock::ist_date(e.0)).collect();
        days.dedup();
        Dataset {
            tf,
            symbols,
            events,
            days,
        }
    }

    /// Loads `data/candles/<tf>/<SYMBOL>.parquet` for each symbol, keeping
    /// bars on IST dates in `[from, to]`. Missing files are skipped with a
    /// warning.
    pub fn load(data_dir: &Path, tf: Tf, symbols: &[Symbol], from: NaiveDate, to: NaiveDate) -> Result<Self> {
        let layout = DataLayout::new(data_dir);
        let mut out = Vec::new();
        for sym in symbols {
            let path = layout.candles(tf, sym);
            if !path.exists() {
                tracing::warn!(symbol = %sym, path = %path.display(), "no candles; skipping");
                continue;
            }
            let bars =
                saarthi_store::parquet::read_bars(&path).with_context(|| format!("reading {}", path.display()))?;
            let bars: Vec<Bar> = bars
                .into_iter()
                .filter(|b| (from..=to).contains(&MarketClock::ist_date(b.ts)))
                .collect();
            out.push(SymbolBars {
                symbol: sym.clone(),
                bars,
            });
        }
        Ok(Dataset::new(tf, out))
    }

    pub fn bar(&self, sym: u32, idx: u32) -> &Bar {
        &self.symbols[sym as usize].bars[idx as usize]
    }

    /// Index range of events whose IST date is in `[from, to]`.
    pub fn event_range(&self, from: NaiveDate, to: NaiveDate) -> std::ops::Range<usize> {
        let start = self.events.partition_point(|e| MarketClock::ist_date(e.0) < from);
        let end = self.events.partition_point(|e| MarketClock::ist_date(e.0) <= to);
        start..end.max(start)
    }

    pub fn bar_count(&self) -> usize {
        self.events.len()
    }
}
