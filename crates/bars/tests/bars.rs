use std::collections::BTreeMap;

use chrono::{NaiveDate, NaiveTime, TimeDelta};
use saarthi_bars::{BarBuilder, BarConfig, BarEvent, BarHub, IndicatorParams, PreOpenPolicy, SymbolState};
use saarthi_core::{Bar, Exchange, MarketClock, Price, Tf, Tick, Ts};
use saarthi_ta::{Atr, Ema, Hlc};

const DAY: &str = "2026-10-27";

fn at(day: &str, h: u32, m: u32, s: u32, ms: u32) -> Ts {
    let d: NaiveDate = day.parse().unwrap();
    MarketClock::at(d, NaiveTime::from_hms_milli_opt(h, m, s, ms).unwrap())
}

fn tick(ts: Ts, ltp: i64, day_volume: u64) -> Tick {
    Tick {
        token: "2885".into(),
        exchange: Exchange::Nse,
        ts,
        ltp: Price(ltp),
        last_qty: Some(1),
        day_volume: Some(day_volume),
        avg_price: None,
    }
}

fn bar(ts: Ts, o: i64, h: i64, l: i64, c: i64, v: u64) -> Bar {
    Bar {
        ts,
        open: Price(o),
        high: Price(h),
        low: Price(l),
        close: Price(c),
        volume: v,
    }
}

fn m1(events: &[BarEvent]) -> Vec<Bar> {
    events.iter().filter(|e| e.tf == Tf::M1).map(|e| e.bar).collect()
}

#[test]
fn builds_one_minute_bars_from_volume_deltas() {
    let mut b = BarBuilder::new(BarConfig::default());
    let mut ev = Vec::new();
    for t in [
        tick(at(DAY, 9, 15, 0, 100), 1000, 500),
        tick(at(DAY, 9, 15, 10, 0), 1010, 700),
        tick(at(DAY, 9, 15, 20, 0), 1010, 700), // quote update, no trade
        tick(at(DAY, 9, 15, 30, 0), 990, 750),
        tick(at(DAY, 9, 15, 59, 999), 995, 760),
        tick(at(DAY, 9, 16, 0, 5), 1001, 800),
        tick(at(DAY, 9, 18, 0, 0), 1002, 900), // 09:17 had no trades
    ] {
        ev.extend(b.on_tick(&t).bars);
    }
    assert_eq!(
        m1(&ev),
        vec![
            bar(at(DAY, 9, 15, 0, 0), 1000, 1010, 990, 995, 760),
            bar(at(DAY, 9, 16, 0, 0), 1001, 1001, 1001, 1001, 40),
        ]
    );
    assert!(ev.iter().all(|e| !e.partial));
}

#[test]
fn quote_updates_without_trades_do_not_open_bars() {
    let mut b = BarBuilder::new(BarConfig::default());
    b.on_tick(&tick(at(DAY, 9, 15, 0, 0), 1000, 100));
    let out = b.on_tick(&tick(at(DAY, 9, 16, 30, 0), 1000, 100));
    assert!(out.trade.is_none());
    assert!(
        out.bars.is_empty(),
        "the 09:15 bar stays open until time or a trade closes it"
    );
    let closed = b.on_time(at(DAY, 9, 16, 2, 0));
    assert_eq!(
        m1(&closed),
        vec![bar(at(DAY, 9, 15, 0, 0), 1000, 1000, 1000, 1000, 100)]
    );
}

#[test]
fn pre_open_auction_folds_into_the_opening_bar() {
    let pre = [
        tick(at(DAY, 9, 7, 30, 0), 1500, 0),
        tick(at(DAY, 9, 8, 1, 0), 1505, 12_000),
    ];
    let open = [
        tick(at(DAY, 9, 15, 0, 300), 1510, 12_100),
        tick(at(DAY, 9, 16, 0, 0), 1511, 12_200),
    ];

    let mut fold = BarBuilder::new(BarConfig::default());
    let mut ev = Vec::new();
    for t in pre.iter().chain(&open) {
        ev.extend(fold.on_tick(t).bars);
    }
    // The 09:07 tick has no volume yet, so its price is yesterday's and is
    // skipped; the auction print at 09:08 opens the bar with 12,000 shares.
    assert_eq!(m1(&ev), vec![bar(at(DAY, 9, 15, 0, 0), 1505, 1510, 1505, 1510, 12_100)]);

    let mut ignore = BarBuilder::new(BarConfig {
        pre_open: PreOpenPolicy::Ignore,
        ..BarConfig::default()
    });
    let mut ev = Vec::new();
    for t in pre.iter().chain(&open) {
        ev.extend(ignore.on_tick(t).bars);
    }
    assert_eq!(m1(&ev), vec![bar(at(DAY, 9, 15, 0, 0), 1510, 1510, 1510, 1510, 100)]);
}

#[test]
fn ticks_outside_the_session_are_ignored() {
    let mut b = BarBuilder::new(BarConfig::default());
    assert!(b.on_tick(&tick(at(DAY, 8, 59, 0, 0), 1, 1)).trade.is_none());
    b.on_tick(&tick(at(DAY, 15, 29, 59, 0), 1000, 100));
    assert!(b.on_tick(&tick(at(DAY, 15, 30, 0, 0), 2000, 200)).trade.is_none());
    let ev = b.flush();
    assert_eq!(m1(&ev)[0].high, Price(1000));
}

#[test]
fn mid_session_start_and_gaps_are_marked_partial() {
    let mut b = BarBuilder::new(BarConfig::default());
    let mut ev = Vec::new();
    // Started at 11:00:30 with 1.5M shares already traded today.
    ev.extend(b.on_tick(&tick(at(DAY, 11, 0, 30, 0), 1000, 1_500_000)).bars);
    ev.extend(b.on_tick(&tick(at(DAY, 11, 0, 40, 0), 1001, 1_500_010)).bars);
    ev.extend(b.on_tick(&tick(at(DAY, 11, 1, 0, 0), 1002, 1_500_020)).bars);
    b.mark_gap();
    // Reconnected two minutes later; 5,000 shares traded in between.
    ev.extend(b.on_tick(&tick(at(DAY, 11, 3, 5, 0), 1003, 1_505_020)).bars);
    ev.extend(b.on_tick(&tick(at(DAY, 11, 3, 6, 0), 1004, 1_505_030)).bars);
    ev.extend(b.flush());
    let got: Vec<_> = ev
        .iter()
        .filter(|e| e.tf == Tf::M1)
        .map(|e| (e.bar.volume, e.partial))
        .collect();
    assert_eq!(got, vec![(10, true), (10, false), (10, true)]);
}

#[test]
fn grace_period_and_late_ticks() {
    let mut b = BarBuilder::new(BarConfig::default());
    b.on_tick(&tick(at(DAY, 9, 20, 10, 0), 1000, 100));
    assert!(b.on_time(at(DAY, 9, 21, 1, 999)).is_empty(), "still in grace");
    let closed = b.on_time(at(DAY, 9, 21, 2, 0));
    assert_eq!(m1(&closed).len(), 1);
    // A 09:20 trade arriving after its bar closed lands in 09:21.
    let out = b.on_tick(&tick(at(DAY, 9, 20, 59, 900), 1005, 110));
    assert_eq!(out.trade.unwrap().ts, at(DAY, 9, 21, 0, 0));
    assert_eq!(b.late_ticks(), 1);
    let ev = b.flush();
    assert_eq!(m1(&ev), vec![bar(at(DAY, 9, 21, 0, 0), 1005, 1005, 1005, 1005, 10)]);
}

#[test]
fn five_minute_bars_roll_up_and_close_on_time() {
    let mut b = BarBuilder::new(BarConfig::default());
    let mut ev = Vec::new();
    let mut vol = 0;
    for (i, m) in [15, 16, 17, 18, 19, 20, 22].into_iter().enumerate() {
        vol += 10;
        ev.extend(b.on_tick(&tick(at(DAY, 9, m, 5, 0), 1000 + i as i64 * 3, vol)).bars);
    }
    let fives: Vec<_> = ev.iter().filter(|e| e.tf == Tf::M5).map(|e| e.bar).collect();
    // Emitted as soon as the 09:19 bar closed, at the first 09:20 trade.
    assert_eq!(fives, vec![bar(at(DAY, 9, 15, 0, 0), 1000, 1012, 1000, 1012, 50)]);
    // 09:20 bucket: 09:23 and 09:24 have no trades, so time closes it.
    let closed = b.on_time(at(DAY, 9, 25, 2, 0));
    let fives: Vec<_> = closed.iter().filter(|e| e.tf == Tf::M5).map(|e| e.bar).collect();
    assert_eq!(fives, vec![bar(at(DAY, 9, 20, 0, 0), 1015, 1018, 1015, 1018, 20)]);
    // 1m bars come before the 5m bar they complete.
    assert_eq!(closed.iter().map(|e| e.tf).collect::<Vec<_>>(), vec![Tf::M1, Tf::M5]);
}

#[test]
fn new_day_resets_the_session() {
    let mut b = BarBuilder::new(BarConfig::default());
    b.on_tick(&tick(at(DAY, 15, 29, 0, 0), 1000, 9_000_000));
    let out = b.on_tick(&tick(at("2026-10-28", 9, 15, 1, 0), 1100, 4000));
    assert!(out.new_session);
    assert_eq!(m1(&out.bars).len(), 1, "yesterday's last bar is flushed");
    let ev = b.flush();
    // The first sight of the day in the opening minute counts the whole day volume.
    assert_eq!(m1(&ev)[0].volume, 4000);
    assert!(
        b.on_tick(&tick(at(DAY, 10, 0, 0, 0), 1, 1)).trade.is_none(),
        "old ticks are dropped"
    );
}

/// A tiny deterministic PRNG so the randomized test needs no dependency.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Brute force: group trades by minute (and by 5-minute bucket from 09:15).
fn reference(trades: &[(Ts, i64, u64)], minutes: i64) -> Vec<Bar> {
    let open = at(DAY, 9, 15, 0, 0);
    let mut buckets: BTreeMap<Ts, Vec<(i64, u64)>> = BTreeMap::new();
    for &(ts, p, q) in trades {
        let k = open + TimeDelta::minutes((ts - open).num_minutes().div_euclid(minutes) * minutes);
        buckets.entry(k).or_default().push((p, q));
    }
    buckets
        .into_iter()
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
fn randomized_full_session_matches_brute_force() {
    for seed in 1..=5u64 {
        let mut rng = Lcg(seed);
        let mut b = BarBuilder::new(BarConfig::default());
        let mut events = Vec::new();
        let mut trades = Vec::new();
        let mut t = at(DAY, 9, 15, 0, 0);
        let close = at(DAY, 15, 30, 0, 0);
        let (mut price, mut vol) = (250_000i64, 0u64);
        while t < close {
            let is_trade = rng.below(4) != 0;
            if is_trade {
                price += rng.below(41) as i64 * 5 - 100;
                let q = 1 + rng.below(500);
                vol += q;
                trades.push((t, price, q));
            }
            events.extend(b.on_tick(&tick(t, price, vol)).bars);
            // Occasionally the wall clock closes bars between ticks.
            if rng.below(10) == 0 {
                events.extend(b.on_time(t + TimeDelta::milliseconds(500)));
            }
            // Gaps of up to ~3 minutes leave some minutes empty.
            let step = if rng.below(200) == 0 {
                60_000 + rng.below(120_000)
            } else {
                rng.below(4_000)
            };
            t += TimeDelta::milliseconds(step as i64 + 1);
        }
        events.extend(b.on_time(close + TimeDelta::seconds(5)));
        events.extend(b.flush());

        let got_1m: Vec<_> = events.iter().filter(|e| e.tf == Tf::M1).map(|e| e.bar).collect();
        let got_5m: Vec<_> = events.iter().filter(|e| e.tf == Tf::M5).map(|e| e.bar).collect();
        assert_eq!(got_1m, reference(&trades, 1), "seed {seed}: 1m");
        assert_eq!(got_5m, reference(&trades, 5), "seed {seed}: 5m");
        assert!(got_1m.len() > 300, "seed {seed}: {} bars", got_1m.len());
        assert!(got_1m.iter().all(Bar::is_consistent));
        assert!(events.iter().all(|e| !e.partial));
        let total: u64 = got_1m.iter().map(|b| b.volume).sum();
        assert_eq!(total, vol, "no volume lost");
    }
}

fn state() -> SymbolState {
    SymbolState::new("2885", BarConfig::default(), IndicatorParams::default())
}

#[test]
fn snapshot_tracks_session_levels() {
    let mut s = state();
    let mut v = 0;
    for (m, p) in [(15, 1000), (20, 1040), (29, 980), (30, 1100)] {
        v += 100;
        s.on_tick(&tick(at(DAY, 9, m, 1, 0), p, v));
    }
    let snap = s.snapshot();
    assert_eq!((snap.or_high, snap.or_low), (Some(Price(1040)), Some(Price(980))));
    assert_eq!(snap.ltp, Some(Price(1100)));
    // Computed VWAP (no exchange ATP in these ticks): mean of the 4 prices.
    assert_eq!(snap.vwap, Some(Price(1030)));
    let (lo, hi) = (snap.vwap_lower.unwrap(), snap.vwap_upper.unwrap());
    assert!(lo < 1030.0 && hi > 1030.0 && ((hi + lo) / 2.0 - 1030.0).abs() < 1e-9);
    assert_eq!(snap.bar_5m.unwrap().ts, at(DAY, 9, 25, 0, 0));

    // With the exchange's ATP present, the snapshot uses it.
    let mut t = tick(at(DAY, 9, 30, 30, 0), 1101, v + 1);
    t.avg_price = Some(Price(1027));
    s.on_tick(&t);
    assert_eq!(s.snapshot().vwap, Some(Price(1027)));
}

#[test]
fn opening_range_completes_on_time_for_quiet_symbols() {
    let mut s = state();
    s.on_tick(&tick(at(DAY, 9, 16, 0, 0), 500, 10));
    assert_eq!(s.snapshot().or_high, None);
    s.on_time(at(DAY, 9, 30, 0, 0));
    assert_eq!(s.snapshot().or_high, Some(Price(500)));
}

#[test]
fn indicators_run_on_five_minute_bars_after_warm_up() {
    let mut s = state();
    let hist: Vec<Bar> = (0..30)
        .map(|i| {
            let p = 1000 + (i % 7) * 10;
            let v = 1000 + (i as u64 % 3) * 10;
            bar(
                at("2026-10-26", 9, 15, 0, 0) + TimeDelta::minutes(5 * i),
                p,
                p + 20,
                p - 20,
                p + 5,
                v,
            )
        })
        .collect();
    s.warm_up(&hist);
    let (mut ef, mut es, mut atr) = (Ema::new(9), Ema::new(21), Atr::new(14));
    for b in &hist {
        ef.update(b.close.0 as f64);
        es.update(b.close.0 as f64);
        atr.update(Hlc {
            high: b.high.0 as f64,
            low: b.low.0 as f64,
            close: b.close.0 as f64,
        });
    }
    assert_eq!(s.snapshot().ema_fast, ef.value());
    assert_eq!(s.snapshot().ema_slow, es.value());
    assert_eq!(s.snapshot().atr, atr.value());
    assert!(s.snapshot().rsi.is_some());
    assert!(s.snapshot().vol_z.unwrap().abs() < 2.0);

    // A live 5m bar moves them; 1m bars do not.
    let before = s.snapshot().ema_fast;
    s.on_tick(&tick(at(DAY, 9, 15, 0, 0), 2000, 50_000));
    s.on_time(at(DAY, 9, 16, 3, 0));
    assert_eq!(s.snapshot().ema_fast, before);
    s.on_time(at(DAY, 9, 20, 3, 0));
    assert_ne!(s.snapshot().ema_fast, before);
    assert!(s.snapshot().vol_z.unwrap() > 10.0, "50k vs a ~1k baseline");
}

#[test]
fn hub_routes_by_token() {
    let mut hub = BarHub::new();
    hub.insert(state());
    hub.insert(SymbolState::new(
        "11536",
        BarConfig::default(),
        IndicatorParams::default(),
    ));
    let mut t = tick(at(DAY, 9, 15, 1, 0), 100, 5);
    t.token = "11536".into();
    assert!(hub.on_tick(&t).is_some());
    t.token = "999".into();
    assert!(hub.on_tick(&t).is_none());
    assert_eq!(hub.snapshot("11536").unwrap().ltp, Some(Price(100)));
    assert_eq!(hub.snapshot("2885").unwrap().ltp, None);
    let closed = hub.on_time(at(DAY, 9, 16, 5, 0));
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].0, "11536");
}

#[test]
fn config_parses_from_toml() {
    let cfg: BarConfig =
        toml::from_str("pre_open = \"ignore\"\nclose_grace_ms = 500\nhigher = [\"5m\", \"15m\"]").unwrap();
    assert_eq!(cfg.pre_open, PreOpenPolicy::Ignore);
    assert_eq!(cfg.higher, vec![Tf::M5, Tf::M15]);
    let p: IndicatorParams = toml::from_str("tf = \"15m\"\nema_fast = 5").unwrap();
    assert_eq!((p.tf, p.ema_fast, p.ema_slow), (Tf::M15, 5, 21));
}

#[test]
fn history_bars_rebuild_session_levels_and_reset_each_day() {
    let mut s = SymbolState::new("2885", BarConfig::default(), IndicatorParams::default());
    let five = |day: &str, h: u32, m: u32, o, hi, lo, c, v| bar(at(day, h, m, 0, 0), o, hi, lo, c, v);
    // Opening range forms over the first three 5m bars (09:15–09:30).
    s.on_history_bar(&five(DAY, 9, 15, 1000, 1010, 995, 1005, 100));
    assert_eq!(s.snapshot().or_high, None, "range still forming");
    s.on_history_bar(&five(DAY, 9, 20, 1005, 1020, 1000, 1015, 100));
    s.on_history_bar(&five(DAY, 9, 25, 1015, 1018, 990, 1000, 100));
    let snap = s.snapshot();
    assert_eq!((snap.or_high, snap.or_low), (Some(Price(1020)), Some(Price(990))));
    // Snapshot time is the bar's close.
    assert_eq!(snap.ts, Some(at(DAY, 9, 30, 0, 0)));
    assert_eq!(snap.ltp, Some(Price(1000)));
    assert_eq!(snap.bar_5m.map(|b| b.close), Some(Price(1000)));
    // VWAP of typical prices (1003.33, 1011.67, 1002.67) with equal volume.
    assert_eq!(snap.vwap, Some(Price(1006)));
    assert!(snap.vwap_sigma.is_some_and(|x| x > 0.0));
    // A breakout bar after the window doesn't move the range.
    s.on_history_bar(&five(DAY, 9, 30, 1000, 1100, 1000, 1090, 400));
    assert_eq!(s.snapshot().or_high, Some(Price(1020)));

    // Next day: session levels reset, bar indicators carry on.
    let ema_before = s.snapshot().ema_fast;
    s.on_history_bar(&five("2026-10-28", 9, 15, 2000, 2010, 1990, 2000, 50));
    let snap = s.snapshot();
    assert_eq!(snap.or_high, None);
    assert_eq!(snap.vwap, Some(Price(2000)));
    assert_eq!(ema_before, None, "EMA 9 not warm after 4 bars");
}
