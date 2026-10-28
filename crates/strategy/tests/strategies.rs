//! Each strategy fires on its setup and stays quiet otherwise.

use chrono::{NaiveDate, NaiveTime, TimeDelta};
use saarthi_bars::Snapshot;
use saarthi_core::{Bar, MarketClock, Price, Side, Ts};
use saarthi_strategy::{BarCtx, Direction, Exit, ParamSet, Signal, Strategy, registry};

fn at(min_after_open: i64) -> Ts {
    let d = NaiveDate::from_ymd_opt(2026, 10, 28).unwrap();
    MarketClock::at(d, NaiveTime::from_hms_opt(9, 15, 0).unwrap()) + TimeDelta::minutes(min_after_open)
}

fn bar(close_min: i64, o: i64, h: i64, l: i64, c: i64) -> Bar {
    Bar {
        ts: at(close_min - 5),
        open: Price(o),
        high: Price(h),
        low: Price(l),
        close: Price(c),
        volume: 1000,
    }
}

fn ctx(b: Bar, close_min: i64) -> BarCtx {
    BarCtx {
        bar: b,
        now: at(close_min),
        since_open: TimeDelta::minutes(close_min),
        entries_allowed: true,
        position: None,
        direction: Direction::Both,
    }
}

fn strat(id: &str) -> Box<dyn Strategy> {
    registry::build(id, &ParamSet::default()).unwrap()
}

fn check(sig: &Signal, side: Side) {
    assert_eq!(sig.side, side);
    assert!(sig.is_well_formed(), "{sig:?}");
    assert!(sig.risk_per_share() > 0);
}

// ---- ORB ----

fn orb_snap(vol_z: f64) -> Snapshot {
    Snapshot {
        token: "T".into(),
        or_high: Some(Price(101_000)),
        or_low: Some(Price(100_000)),
        atr: Some(300.0),
        vol_z: Some(vol_z),
        ..Snapshot::default()
    }
}

#[test]
fn orb_breaks_out_once_per_day() {
    let mut s = strat("orb");
    let snap = orb_snap(2.0);
    // Inside the range first, then a close above it.
    assert!(
        s.on_bar(&snap, &ctx(bar(20, 100_500, 100_800, 100_400, 100_700), 20))
            .is_none()
    );
    let sig = s
        .on_bar(&snap, &ctx(bar(25, 100_700, 101_300, 100_700, 101_200), 25))
        .unwrap();
    check(&sig, Side::Buy);
    // Defaults: stop 1.5 ATR, target 2R.
    assert_eq!(sig.stop, Price(101_200 - 450));
    assert_eq!(sig.target, Some(Price(101_200 + 900)));
    // Back inside and out again the same day: no second trade.
    s.on_bar(&snap, &ctx(bar(30, 101_200, 101_200, 100_500, 100_600), 30));
    assert!(
        s.on_bar(&snap, &ctx(bar(35, 100_600, 101_500, 100_600, 101_400), 35))
            .is_none()
    );
}

#[test]
fn orb_needs_volume_time_and_permission() {
    let inside = bar(20, 100_500, 100_800, 100_400, 100_700);
    let out = bar(25, 100_700, 101_300, 100_700, 101_200);
    // Weak volume.
    let mut s = strat("orb");
    s.on_bar(&orb_snap(0.2), &ctx(inside, 20));
    assert!(s.on_bar(&orb_snap(0.2), &ctx(out, 25)).is_none());
    // Too late in the day (default last entry 120 min after open).
    let mut s = strat("orb");
    s.on_bar(&orb_snap(2.0), &ctx(inside, 200));
    assert!(s.on_bar(&orb_snap(2.0), &ctx(out, 205)).is_none());
    // Short-only plan.
    let mut s = strat("orb");
    s.on_bar(&orb_snap(2.0), &ctx(inside, 20));
    let mut c = ctx(out, 25);
    c.direction = Direction::ShortOnly;
    assert!(s.on_bar(&orb_snap(2.0), &c).is_none());
    // Entries blocked by risk.
    let mut s = strat("orb");
    s.on_bar(&orb_snap(2.0), &ctx(inside, 20));
    let mut c = ctx(out, 25);
    c.entries_allowed = false;
    assert!(s.on_bar(&orb_snap(2.0), &c).is_none());
}

#[test]
fn orb_breaks_down_short() {
    let mut s = strat("orb");
    s.on_bar(&orb_snap(2.0), &ctx(bar(20, 100_500, 100_800, 100_400, 100_500), 20));
    let sig = s
        .on_bar(&orb_snap(2.0), &ctx(bar(25, 100_500, 100_500, 99_700, 99_800), 25))
        .unwrap();
    check(&sig, Side::Sell);
    assert!(sig.stop > sig.entry && sig.target.unwrap() < sig.entry);
}

// ---- VWAP reversion ----

fn range_snap(rsi: f64) -> Snapshot {
    Snapshot {
        token: "T".into(),
        vwap: Some(Price(100_000)),
        vwap_sigma: Some(200.0),
        atr: Some(250.0),
        ema_fast: Some(100_010.0),
        ema_slow: Some(100_000.0),
        rsi: Some(rsi),
        ..Snapshot::default()
    }
}

#[test]
fn vwap_revert_fades_a_stretch_back_inside_the_band() {
    let mut s = strat("vwap_revert");
    // 2σ lower band = 999.00. Close below it...
    assert!(
        s.on_bar(&range_snap(25.0), &ctx(bar(60, 99_700, 99_700, 99_500, 99_550), 60))
            .is_none()
    );
    // ...then back inside: long, target VWAP, stop below the stretch low.
    let sig = s
        .on_bar(&range_snap(35.0), &ctx(bar(65, 99_550, 99_950, 99_540, 99_920), 65))
        .unwrap();
    check(&sig, Side::Buy);
    assert_eq!(sig.target, Some(Price(100_000)));
    assert_eq!(sig.stop, Price(99_500 - 125)); // low − 0.5 ATR
}

#[test]
fn vwap_revert_stays_out_of_trends_and_the_open() {
    let mut trend = range_snap(35.0);
    trend.ema_fast = Some(100_400.0); // 1.6 ATR apart
    let mut s = strat("vwap_revert");
    s.on_bar(&trend, &ctx(bar(60, 99_700, 99_700, 99_500, 99_550), 60));
    assert!(
        s.on_bar(&trend, &ctx(bar(65, 99_550, 99_950, 99_540, 99_920), 65))
            .is_none()
    );
    // Before first_entry_min (30).
    let mut s = strat("vwap_revert");
    s.on_bar(&range_snap(25.0), &ctx(bar(15, 99_700, 99_700, 99_500, 99_550), 15));
    assert!(
        s.on_bar(&range_snap(35.0), &ctx(bar(20, 99_550, 99_950, 99_540, 99_920), 20))
            .is_none()
    );
}

#[test]
fn vwap_revert_exits_when_the_range_turns_into_a_trend() {
    let mut s = strat("vwap_revert");
    let mut c = ctx(bar(70, 100_000, 100_100, 99_900, 100_000), 70);
    c.position = Some(Side::Buy);
    assert_eq!(s.manage(&range_snap(50.0), &c), Exit::Hold);
    let mut trend = range_snap(50.0);
    trend.ema_fast = Some(100_400.0);
    assert!(matches!(s.manage(&trend, &c), Exit::Close(_)));
}

// ---- Momentum ----

fn trend_snap() -> Snapshot {
    Snapshot {
        token: "T".into(),
        vwap: Some(Price(100_000)),
        ema_fast: Some(101_000.0),
        ema_slow: Some(100_800.0),
        atr: Some(200.0), // separation 1.0 ATR
        ..Snapshot::default()
    }
}

#[test]
fn momentum_buys_a_pullback_to_the_fast_ema_in_an_uptrend() {
    let mut s = strat("momentum");
    let sig = s
        .on_bar(&trend_snap(), &ctx(bar(60, 101_000, 101_200, 100_990, 101_150), 60))
        .unwrap();
    check(&sig, Side::Buy);
    // Stop below min(low, slow EMA) by 0.5 ATR: 1008.00 − 1.00.
    assert_eq!(sig.stop, Price(100_700));
    // A bar that never came near the EMA is not a pullback.
    let mut s = strat("momentum");
    assert!(
        s.on_bar(&trend_snap(), &ctx(bar(60, 101_500, 101_700, 101_400, 101_650), 60))
            .is_none()
    );
    // A red bar is not a resumption.
    let mut s = strat("momentum");
    assert!(
        s.on_bar(&trend_snap(), &ctx(bar(60, 101_150, 101_200, 100_990, 101_050), 60))
            .is_none()
    );
}

#[test]
fn momentum_needs_a_trend_and_exits_when_it_breaks() {
    let mut flat = trend_snap();
    flat.ema_fast = Some(100_820.0);
    let mut s = strat("momentum");
    assert!(
        s.on_bar(&flat, &ctx(bar(60, 101_000, 101_200, 100_810, 101_150), 60))
            .is_none()
    );

    let mut c = ctx(bar(90, 100_900, 100_900, 100_600, 100_700), 90);
    c.position = Some(Side::Buy);
    assert!(
        matches!(s.manage(&trend_snap(), &c), Exit::Close(_)),
        "closed below slow EMA"
    );
    c.bar.close = Price(100_900);
    assert_eq!(s.manage(&trend_snap(), &c), Exit::Hold);
}

#[test]
fn no_entries_while_in_a_position() {
    for id in registry::IDS {
        let mut s = strat(id);
        let mut c = ctx(bar(60, 101_000, 101_200, 100_990, 101_150), 60);
        c.position = Some(Side::Buy);
        assert!(s.on_bar(&trend_snap(), &c).is_none(), "{id}");
    }
}
