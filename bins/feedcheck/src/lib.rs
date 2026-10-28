//! Library half of `feedcheck`: collecting live bars, replaying recordings
//! and comparing bars with Angel's historical candles.

pub mod compare;
pub mod config;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use saarthi_bars::{BarConfig, BarHub, IndicatorParams, SymbolState};
use saarthi_core::{Bar, Symbol, Tf, Ts};
use saarthi_feed::{Packet, decode};
use saarthi_store::DataLayout;
use saarthi_store::parquet::write_bars;

/// Name of the file listing partial bars next to a session's 1m bars.
pub const PARTIAL_FILE: &str = "_partial.json";

/// Builds and keeps every 1m bar for a set of symbols.
pub struct Collector {
    hub: BarHub,
    symbols: HashMap<String, Symbol>,
    bars: BTreeMap<Symbol, Vec<Bar>>,
    partial: BTreeMap<Symbol, BTreeSet<Ts>>,
    pub packets: u64,
    pub decode_errors: u64,
}

impl Collector {
    /// `symbols` maps feed token to trading symbol.
    pub fn new(symbols: impl IntoIterator<Item = (String, Symbol)>, bars: &BarConfig) -> Self {
        let mut hub = BarHub::new();
        let symbols: HashMap<String, Symbol> = symbols.into_iter().collect();
        for tok in symbols.keys() {
            hub.insert(SymbolState::new(tok.clone(), bars.clone(), IndicatorParams::default()));
        }
        Collector {
            hub,
            symbols,
            bars: BTreeMap::new(),
            partial: BTreeMap::new(),
            packets: 0,
            decode_errors: 0,
        }
    }

    pub fn hub(&self) -> &BarHub {
        &self.hub
    }

    fn keep(&mut self, token: &str, events: impl IntoIterator<Item = saarthi_bars::BarEvent>) {
        let Some(sym) = self.symbols.get(token) else { return };
        for ev in events.into_iter().filter(|e| e.tf == Tf::M1) {
            if ev.partial {
                self.partial.entry(sym.clone()).or_default().insert(ev.bar.ts);
            }
            self.bars.entry(sym.clone()).or_default().push(ev.bar);
        }
    }

    pub fn on_packet(&mut self, p: &Packet) {
        self.packets += 1;
        let Some(tick) = p.tick() else { return };
        if let Some(u) = self.hub.on_tick(&tick) {
            self.keep(&tick.token, u.bars);
        }
    }

    pub fn on_frame(&mut self, frame: &[u8]) {
        match decode(frame) {
            Ok(p) => self.on_packet(&p),
            Err(_) => self.decode_errors += 1,
        }
    }

    pub fn on_time(&mut self, now: Ts) {
        for (tok, ev) in self.hub.on_time(now) {
            self.keep(&tok, [ev]);
        }
    }

    /// The feed dropped: the next bar of every symbol may be missing trades.
    pub fn on_disconnect(&mut self) {
        self.hub.mark_gap();
    }

    pub fn flush(&mut self) {
        for (tok, ev) in self.hub.flush() {
            self.keep(&tok, [ev]);
        }
    }

    pub fn bars(&self) -> &BTreeMap<Symbol, Vec<Bar>> {
        &self.bars
    }

    pub fn partial(&self) -> &BTreeMap<Symbol, BTreeSet<Ts>> {
        &self.partial
    }

    /// Writes each symbol's 1m bars and the partial-bar list for `date`.
    pub fn save(&self, layout: &DataLayout, date: NaiveDate) -> Result<usize> {
        for (sym, bars) in &self.bars {
            let path = layout.live_bars(date, Tf::M1, sym);
            write_bars(&path, bars).with_context(|| format!("writing {}", path.display()))?;
        }
        let dir = layout.live_dir(date).join(Tf::M1.as_str());
        std::fs::create_dir_all(&dir)?;
        let partial: BTreeMap<&str, Vec<String>> = self
            .partial
            .iter()
            .map(|(s, ts)| (s.as_str(), ts.iter().map(|t| t.to_rfc3339()).collect()))
            .collect();
        std::fs::write(dir.join(PARTIAL_FILE), serde_json::to_vec_pretty(&partial)?)?;
        Ok(self.bars.len())
    }
}

/// Replays recorded frames, using each frame's receive time as the clock.
pub fn replay(frames: &[(Ts, Vec<u8>)], collector: &mut Collector) {
    for (received, frame) in frames {
        collector.on_time(*received);
        collector.on_frame(frame);
    }
    collector.flush();
}

/// Reads the partial-bar list written by [`Collector::save`].
pub fn load_partial(layout: &DataLayout, date: NaiveDate) -> Result<BTreeMap<Symbol, BTreeSet<Ts>>> {
    let path = layout.live_dir(date).join(Tf::M1.as_str()).join(PARTIAL_FILE);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw: BTreeMap<String, Vec<String>> = serde_json::from_slice(&std::fs::read(&path)?)?;
    raw.into_iter()
        .map(|(s, ts)| {
            let set = ts
                .iter()
                .map(|t| Ok(chrono::DateTime::parse_from_rfc3339(t)?.to_utc()))
                .collect::<Result<BTreeSet<_>>>()?;
            Ok((Symbol::new(s), set))
        })
        .collect()
}
