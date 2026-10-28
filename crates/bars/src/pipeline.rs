use std::collections::HashMap;

use saarthi_core::{Bar, Price, Tf, Tick, Ts};
use saarthi_ta::{Atr, Ema, Hlc, Rsi, VolumeZScore, VwapBands};
use serde::{Deserialize, Serialize};

use crate::builder::{BarBuilder, BarConfig, BarEvent};
use crate::session::{OpeningRange, SessionVwap};

/// Indicator settings. The defaults are the ones ARCHITECTURE §5.4 names:
/// EMA 9/21, ATR 14, RSI 14 on 5-minute bars, 2σ VWAP bands, a 20-bar volume
/// z-score and a 15-minute opening range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndicatorParams {
    /// Timeframe the bar-based indicators run on: 1m or one of
    /// [`BarConfig::higher`].
    pub tf: Tf,
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub atr: usize,
    pub rsi: usize,
    pub volume_z: usize,
    pub vwap_band_k: f64,
    pub opening_range_minutes: u32,
}

impl Default for IndicatorParams {
    fn default() -> Self {
        IndicatorParams {
            tf: Tf::M5,
            ema_fast: 9,
            ema_slow: 21,
            atr: 14,
            rsi: 14,
            volume_z: 20,
            vwap_band_k: 2.0,
            opening_range_minutes: 15,
        }
    }
}

/// Everything a strategy reads about one symbol.
///
/// Observed levels (prices, bars, the opening range) are exact [`Price`]s.
/// Computed indicators are `f64` **in paise**, unrounded, so crossovers are
/// not distorted by rounding to the paisa.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub token: String,
    /// Exchange time of the last tick.
    pub ts: Option<Ts>,
    pub ltp: Option<Price>,
    /// Last completed 1-minute bar.
    pub bar_1m: Option<Bar>,
    /// Last completed 5-minute bar (if 5m bars are built).
    pub bar_5m: Option<Bar>,
    /// Session VWAP: the exchange's own average traded price when the feed
    /// carries it, otherwise computed from the trades seen.
    pub vwap: Option<Price>,
    pub vwap_upper: Option<f64>,
    pub vwap_lower: Option<f64>,
    pub ema_fast: Option<f64>,
    pub ema_slow: Option<f64>,
    pub atr: Option<f64>,
    pub rsi: Option<f64>,
    /// Volume z-score of the last completed indicator bar.
    pub vol_z: Option<f64>,
    /// Opening range, once its window has closed.
    pub or_high: Option<Price>,
    pub or_low: Option<Price>,
}

/// What one tick changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Update {
    pub new_session: bool,
    pub bars: Vec<BarEvent>,
}

/// Bars, session levels and indicators for one instrument.
#[derive(Debug, Clone)]
pub struct SymbolState {
    params: IndicatorParams,
    builder: BarBuilder,
    or: OpeningRange,
    vwap: SessionVwap,
    bands: VwapBands,
    ema_fast: Ema,
    ema_slow: Ema,
    atr: Atr,
    rsi: Rsi,
    vol_z: VolumeZScore,
    snap: Snapshot,
}

impl SymbolState {
    pub fn new(token: impl Into<String>, bars: BarConfig, params: IndicatorParams) -> Self {
        assert!(
            params.tf == Tf::M1 || bars.higher.contains(&params.tf),
            "indicator timeframe {} is not being built",
            params.tf
        );
        SymbolState {
            builder: BarBuilder::new(bars),
            or: OpeningRange::new(params.opening_range_minutes),
            vwap: SessionVwap::default(),
            bands: VwapBands::new(),
            ema_fast: Ema::new(params.ema_fast),
            ema_slow: Ema::new(params.ema_slow),
            atr: Atr::new(params.atr),
            rsi: Rsi::new(params.rsi),
            vol_z: VolumeZScore::new(params.volume_z),
            snap: Snapshot {
                token: token.into(),
                ..Snapshot::default()
            },
            params,
        }
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snap
    }

    pub fn builder(&self) -> &BarBuilder {
        &self.builder
    }

    pub fn opening_range(&self) -> &OpeningRange {
        &self.or
    }

    pub fn mark_gap(&mut self) {
        self.builder.mark_gap();
    }

    /// Primes the bar indicators with history on the indicator timeframe
    /// (e.g. the last few sessions of 5m candles from `fetch`), oldest first.
    /// Session levels are not touched.
    pub fn warm_up(&mut self, bars: &[Bar]) {
        for b in bars {
            self.apply_indicator_bar(b, false);
        }
    }

    /// After a restart mid-session: rebuilds today's opening range and the
    /// computed VWAP bands from today's 1m bars (typical price × volume). Call
    /// after the first tick of the day, so the session is known.
    pub fn seed_session(&mut self, bars_1m: &[Bar]) {
        self.or.seed_from_bars(bars_1m);
        for b in bars_1m {
            let tp = (b.high.0 + b.low.0 + b.close.0) as f64 / 3.0;
            self.bands.update(tp, b.volume as f64);
        }
    }

    pub fn on_tick(&mut self, t: &Tick) -> Update {
        let out = self.builder.on_tick(t);
        if out.new_session {
            self.vwap.reset();
            self.bands.reset();
            if let Some(open) = self.builder.session_open() {
                self.or.reset(open);
            }
            self.snap.or_high = None;
            self.snap.or_low = None;
        }
        self.snap.ts = Some(t.ts);
        if let Some(tr) = out.trade {
            self.snap.ltp = Some(tr.price);
            self.vwap.on_trade(tr.price, tr.qty);
            self.bands.update(tr.price.0 as f64, tr.qty as f64);
            self.or.on_trade(tr.ts, tr.price);
        }
        if let Some(atp) = t.avg_price.filter(|p| p.0 > 0) {
            self.snap.vwap = Some(atp);
        } else {
            self.snap.vwap = self.vwap.vwap().or(self.snap.vwap);
        }
        self.refresh_session_levels();
        for ev in &out.bars {
            self.on_bar(ev);
        }
        Update {
            new_session: out.new_session,
            bars: out.bars,
        }
    }

    pub fn on_time(&mut self, now: Ts) -> Vec<BarEvent> {
        self.or.on_time(now);
        self.refresh_session_levels();
        let bars = self.builder.on_time(now);
        for ev in &bars {
            self.on_bar(ev);
        }
        bars
    }

    pub fn flush(&mut self) -> Vec<BarEvent> {
        let bars = self.builder.flush();
        for ev in &bars {
            self.on_bar(ev);
        }
        bars
    }

    fn refresh_session_levels(&mut self) {
        if let Some((hi, lo)) = self.or.range() {
            self.snap.or_high = Some(hi);
            self.snap.or_low = Some(lo);
        }
        let k = self.params.vwap_band_k;
        // Bands are centred on the snapshot VWAP so they agree with it.
        if let (Some(v), Some(s)) = (self.snap.vwap, self.bands.sigma()) {
            self.snap.vwap_upper = Some(v.0 as f64 + k * s);
            self.snap.vwap_lower = Some(v.0 as f64 - k * s);
        }
    }

    fn on_bar(&mut self, ev: &BarEvent) {
        match ev.tf {
            Tf::M1 => self.snap.bar_1m = Some(ev.bar),
            Tf::M5 => self.snap.bar_5m = Some(ev.bar),
            _ => {}
        }
        if ev.tf == self.params.tf {
            self.apply_indicator_bar(&ev.bar, ev.partial);
        }
    }

    fn apply_indicator_bar(&mut self, b: &Bar, partial: bool) {
        let close = b.close.0 as f64;
        self.snap.ema_fast = self.ema_fast.update(close);
        self.snap.ema_slow = self.ema_slow.update(close);
        self.snap.atr = self.atr.update(Hlc {
            high: b.high.0 as f64,
            low: b.low.0 as f64,
            close,
        });
        self.snap.rsi = self.rsi.update(close);
        // A partial bar's volume is incomplete; keep it out of the baseline.
        self.snap.vol_z = if partial { None } else { self.vol_z.update(b.volume) };
    }
}

/// One [`SymbolState`] per subscribed token.
#[derive(Debug, Default)]
pub struct BarHub {
    symbols: HashMap<String, SymbolState>,
}

impl BarHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, state: SymbolState) {
        self.symbols.insert(state.snapshot().token.clone(), state);
    }

    pub fn get(&self, token: &str) -> Option<&SymbolState> {
        self.symbols.get(token)
    }

    pub fn get_mut(&mut self, token: &str) -> Option<&mut SymbolState> {
        self.symbols.get_mut(token)
    }

    pub fn snapshot(&self, token: &str) -> Option<&Snapshot> {
        self.symbols.get(token).map(SymbolState::snapshot)
    }

    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.symbols.keys().map(String::as_str)
    }

    /// Routes a tick to its symbol. Ticks for unknown tokens are ignored.
    pub fn on_tick(&mut self, t: &Tick) -> Option<Update> {
        self.symbols.get_mut(&t.token).map(|s| s.on_tick(t))
    }

    pub fn on_time(&mut self, now: Ts) -> Vec<(String, BarEvent)> {
        let mut out = Vec::new();
        for (tok, s) in &mut self.symbols {
            out.extend(s.on_time(now).into_iter().map(|b| (tok.clone(), b)));
        }
        out
    }

    pub fn flush(&mut self) -> Vec<(String, BarEvent)> {
        let mut out = Vec::new();
        for (tok, s) in &mut self.symbols {
            out.extend(s.flush().into_iter().map(|b| (tok.clone(), b)));
        }
        out
    }

    /// The feed reconnected: every symbol may have missed trades.
    pub fn mark_gap(&mut self) {
        for s in self.symbols.values_mut() {
            s.mark_gap();
        }
    }
}
