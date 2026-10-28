//! A full synthetic session through the real path: binary frames on disk →
//! decode → bar builder → Parquet → compare with candles built independently
//! from the same trades.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use feedcheck::compare::compare;
use feedcheck::{Collector, load_partial, replay};
use saarthi_bars::BarConfig;
use saarthi_core::{Bar, MarketClock, Price, Symbol, Tf, Ts};
use saarthi_feed::record::{Recorder, read_frames};
use saarthi_feed::wire::Quote;
use saarthi_feed::{ExchangeType, Mode, Packet, encode};
use saarthi_store::DataLayout;
use saarthi_store::parquet::read_bars;

struct Lcg(u64);
impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

fn date() -> NaiveDate {
    "2026-10-27".parse().unwrap()
}

fn at(h: u32, m: u32) -> Ts {
    MarketClock::at(date(), NaiveTime::from_hms_opt(h, m, 0).unwrap())
}

struct Sim {
    frames: Vec<(Ts, Vec<u8>)>,
    trades: BTreeMap<String, Vec<(Ts, i64, u64)>>,
}

/// Interleaved Quote packets for several tokens from 09:15 to 15:30,
/// including quote-only updates that carry no new trade.
fn simulate(tokens: &[&str], seed: u64) -> Sim {
    let mut rng = Lcg(seed);
    let mut state: BTreeMap<&str, (i64, i64, i128)> = tokens.iter().map(|t| (*t, (150_000, 0, 0))).collect();
    let mut trades: BTreeMap<String, Vec<_>> = BTreeMap::new();
    let mut frames = Vec::new();
    let mut ts = at(9, 15);
    let mut seq = 0;
    while ts < at(15, 30) {
        let tok = tokens[rng.below(tokens.len() as u64) as usize];
        let (price, vol, pv) = state.get_mut(tok).unwrap();
        if rng.below(5) != 0 {
            *price += rng.below(9) as i64 * 5 - 20;
            let q = 1 + rng.below(300) as i64;
            *vol += q;
            *pv += i128::from(*price) * i128::from(q);
            trades.entry(tok.to_string()).or_default().push((ts, *price, q as u64));
        }
        seq += 1;
        let atp = if *vol > 0 { (*pv / i128::from(*vol)) as i64 } else { 0 };
        let p = Packet {
            mode: Mode::Quote,
            exchange_type: ExchangeType::NseCm,
            token: tok.to_string(),
            sequence: seq,
            exchange_ts: ts,
            ltp: Price(*price),
            quote: Some(Quote {
                last_qty: 1,
                avg_price: Price(atp),
                day_volume: *vol,
                total_buy_qty: 1e5,
                total_sell_qty: 1e5,
                open: Price(150_000),
                high: Price(*price),
                low: Price(*price),
                close: Price(149_000),
            }),
            snap: None,
        };
        frames.push((ts + TimeDelta::milliseconds(15), encode(&p)));
        ts += TimeDelta::milliseconds(1 + rng.below(1_500) as i64);
    }
    Sim { frames, trades }
}

/// Angel-style candles computed straight from the trades.
fn candles(trades: &[(Ts, i64, u64)]) -> Vec<Bar> {
    let mut m: BTreeMap<Ts, Vec<(i64, u64)>> = BTreeMap::new();
    for &(ts, p, q) in trades {
        let k = DateTime::<Utc>::from_timestamp(ts.timestamp() / 60 * 60, 0).unwrap();
        m.entry(k).or_default().push((p, q));
    }
    m.into_iter()
        .map(|(ts, v)| Bar {
            ts,
            open: Price(v[0].0),
            high: Price(v.iter().map(|x| x.0).max().unwrap()),
            low: Price(v.iter().map(|x| x.0).min().unwrap()),
            close: Price(v.last().unwrap().0),
            volume: v.iter().map(|x| x.1).sum(),
        })
        .collect()
}

#[test]
fn recorded_session_replays_into_bars_that_match_candles() {
    let sim = simulate(&["2885", "11536"], 42);
    let dir = tempfile::tempdir().unwrap();
    let layout = DataLayout::new(dir.path());
    {
        let mut rec = Recorder::open(&layout.live_frames(date())).unwrap();
        for (ts, f) in &sim.frames {
            rec.write(*ts, f).unwrap();
        }
    }
    let frames = read_frames(&layout.live_frames(date())).unwrap();
    assert_eq!(frames.len(), sim.frames.len());

    let symbols = [("2885", "RELIANCE-EQ"), ("11536", "TCS-EQ")];
    let mut col = Collector::new(
        symbols.iter().map(|(t, s)| (t.to_string(), Symbol::new(*s))),
        &BarConfig::default(),
    );
    replay(&frames, &mut col);
    assert_eq!(col.decode_errors, 0);
    assert_eq!(col.save(&layout, date()).unwrap(), 2);

    // The exchange's ATP drives the snapshot VWAP.
    let snap = col.hub().snapshot("2885").unwrap();
    assert!(snap.vwap.is_some() && snap.or_high.is_some() && snap.ema_fast.is_some());

    let partial = load_partial(&layout, date()).unwrap();
    assert!(partial.is_empty());
    for (tok, sym) in symbols {
        let sym = Symbol::new(sym);
        let live = read_bars(&layout.live_bars(date(), Tf::M1, &sym)).unwrap();
        let angel = candles(&sim.trades[tok]);
        assert!(live.len() > 350, "{sym}: {} bars", live.len());
        let r = compare(sym.clone(), &live, &angel, 0, &BTreeSet::new(), at(9, 15), at(15, 30));
        assert!(r.passes(Some(0.0)), "{sym}: {r:?}");
        assert_eq!(r.compared, angel.len());

        // A candle two ticks off is caught.
        let mut off = angel.clone();
        off[100].high = Price(off[100].high.0 + 10);
        let r = compare(sym, &live, &off, 5, &BTreeSet::new(), at(9, 15), at(15, 30));
        assert_eq!(r.price_mismatches.len(), 1);
        assert!(!r.passes(None));
    }
}

#[test]
fn feed_gap_marks_bars_partial_and_compare_skips_them() {
    let sim = simulate(&["2885"], 7);
    let mut col = Collector::new(
        [("2885".to_string(), Symbol::new("RELIANCE-EQ"))],
        &BarConfig::default(),
    );
    // Drop everything between 11:00 and 11:05, as if the socket was down.
    let mut dropped = false;
    for (ts, f) in &sim.frames {
        col.on_time(*ts);
        let exchange_ts = saarthi_feed::decode(f).unwrap().exchange_ts;
        if exchange_ts >= at(11, 0) && exchange_ts < at(11, 5) {
            if !dropped {
                col.on_disconnect();
                dropped = true;
            }
            continue;
        }
        col.on_frame(f);
    }
    col.flush();
    let sym = Symbol::new("RELIANCE-EQ");
    let partial = &col.partial()[&sym];
    assert_eq!(partial.iter().copied().collect::<Vec<_>>(), vec![at(11, 5)]);
    let live = &col.bars()[&sym];
    let angel = candles(&sim.trades["2885"]);
    let r = compare(sym.clone(), live, &angel, 5, partial, at(9, 15), at(15, 30));
    // The five dropped minutes are genuinely missing; nothing else is wrong.
    assert_eq!(r.missing_live, (0..5).map(|m| at(11, m)).collect::<Vec<_>>());
    assert!(r.price_mismatches.is_empty());
    assert_eq!(r.excluded, 1);
}
