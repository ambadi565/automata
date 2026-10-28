//! Every hard limit, breached: `cargo test -p saarthi-risk -- breach`.
//!
//! Amounts are written rupees_paise: `1000_00` is ₹1000.00.

#![allow(clippy::inconsistent_digit_grouping)]

use chrono::{NaiveDate, NaiveTime, TimeDelta};
use saarthi_core::{MarketClock, Phase, Price, Side, Ts};
use saarthi_risk::{Book, Breach, EntryProposal, HaltReason, PriceBand, Rejection, RiskGuard, RiskLimits};

fn limits() -> RiskLimits {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/risk.toml");
    RiskLimits::load(std::path::Path::new(p)).unwrap()
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
}

fn at(h: u32, m: u32) -> Ts {
    MarketClock::at(day(), NaiveTime::from_hms_opt(h, m, 0).unwrap())
}

/// ₹10 lakh capital.
const CAPITAL: Price = Price(10_00_000_00);

fn guard() -> RiskGuard {
    let mut g = RiskGuard::new(limits());
    g.begin_day(day(), CAPITAL);
    g
}

fn book() -> Book {
    Book {
        phase: Phase::Open,
        feed_ok: true,
        open_positions: 0,
        gross_exposure: Price::ZERO,
        symbol_busy: false,
    }
}

/// Long at ₹1000 with a ₹10 stop.
fn prop(token: &str) -> EntryProposal<'_> {
    EntryProposal {
        now: at(10, 0),
        token,
        strategy: "orb",
        side: Side::Buy,
        entry: Price(1000_00),
        stop: Price(990_00),
        ltp: Some(Price(1000_00)),
        band: None,
        risk_scale: 1.0,
    }
}

#[test]
fn sizing_respects_risk_per_trade() {
    let mut g = guard();
    let a = g.check(&prop("1"), &book()).unwrap();
    // 0.5 % of ₹10 L = ₹5000; ₹10 per share → 500 shares.
    assert_eq!(a.qty, 500);
    assert_eq!(a.risk, Price(5000_00));
    // Half risk scale → half size.
    let mut g = guard();
    let p = EntryProposal {
        risk_scale: 0.5,
        ..prop("1")
    };
    assert_eq!(g.check(&p, &book()).unwrap().qty, 250);
    // A plan cannot scale risk above 1.
    let mut g = guard();
    let p = EntryProposal {
        risk_scale: 7.0,
        ..prop("1")
    };
    assert_eq!(g.check(&p, &book()).unwrap().qty, 500);
}

#[test]
fn breach_kill_switch_blocks_entries() {
    let mut g = guard();
    g.halt(HaltReason::Manual("telegram".into()));
    assert!(matches!(g.check(&prop("1"), &book()), Err(Rejection::Halted(_))));
    g.resume(Phase::Open).unwrap();
    assert!(g.check(&prop("1"), &book()).is_ok());
}

#[test]
fn breach_resume_refused_after_cutoff_and_after_daily_loss() {
    let mut g = guard();
    g.halt(HaltReason::Manual("x".into()));
    assert!(g.resume(Phase::NoNewEntries).is_err());
    let mut g = guard();
    assert_eq!(g.on_pnl(Price(-20_000_00)), Some(Breach::FlattenAndHalt));
    assert!(g.resume(Phase::Open).is_err());
    // A later manual halt cannot downgrade the daily-loss halt into a resumable one.
    g.halt(HaltReason::Manual("y".into()));
    assert!(g.resume(Phase::Open).is_err());
}

#[test]
fn breach_phase_blocks_entries_outside_open() {
    for phase in [Phase::PreOpen, Phase::NoNewEntries, Phase::SquareOff, Phase::Closed] {
        let mut g = guard();
        let b = Book { phase, ..book() };
        assert!(matches!(g.check(&prop("1"), &b), Err(Rejection::Phase(_))), "{phase:?}");
    }
    // A guard that has not started today refuses too.
    let mut g = RiskGuard::new(limits());
    assert!(matches!(g.check(&prop("1"), &book()), Err(Rejection::Phase(_))));
}

#[test]
fn breach_stale_feed_blocks_entries() {
    let mut g = guard();
    let b = Book {
        feed_ok: false,
        ..book()
    };
    assert_eq!(g.check(&prop("1"), &b), Err(Rejection::FeedUnhealthy));
}

#[test]
fn breach_daily_loss_flattens_and_halts_once() {
    let mut g = guard();
    // 2 % of ₹10 L = ₹20 000.
    assert_eq!(g.daily_loss_limit(), Price(20_000_00));
    assert_eq!(g.on_pnl(Price(-19_999_99)), None);
    assert_eq!(g.on_pnl(Price(-20_000_00)), Some(Breach::FlattenAndHalt));
    assert_eq!(g.on_pnl(Price(-25_000_00)), None, "fires once");
    assert_eq!(g.check(&prop("1"), &book()), Err(Rejection::DailyLossHit));
    // Next day trading resumes with a fresh capital base.
    g.begin_day(day().succ_opt().unwrap(), Price(9_80_000_00));
    assert!(g.halted().is_none());
}

#[test]
fn breach_manual_halt_survives_the_day_roll() {
    let mut g = guard();
    g.halt(HaltReason::Manual("emergency".into()));
    g.begin_day(day().succ_opt().unwrap(), CAPITAL);
    assert!(matches!(g.halted(), Some(HaltReason::Manual(_))));
}

#[test]
fn breach_profit_lock_stops_new_entries() {
    let l = RiskLimits {
        daily_profit_lock_pct: 1.0,
        ..limits()
    };
    let mut g = RiskGuard::new(l);
    g.begin_day(day(), CAPITAL);
    assert_eq!(g.on_pnl(Price(9_999_00)), None);
    assert_eq!(g.on_pnl(Price(10_000_00)), Some(Breach::ProfitLocked));
    assert_eq!(g.check(&prop("1"), &book()), Err(Rejection::ProfitLocked));
    // Off by default: no amount of profit locks.
    let mut g = guard();
    assert_eq!(g.on_pnl(Price(1_00_000_00)), None);
}

#[test]
fn breach_max_trades_per_day() {
    let mut g = guard();
    let n = limits().max_trades_per_day;
    for i in 0..n {
        let tok = i.to_string();
        g.check(&prop(&tok), &book()).unwrap();
    }
    assert_eq!(g.check(&prop("x"), &book()), Err(Rejection::MaxTradesPerDay));
}

#[test]
fn breach_max_open_positions() {
    let mut g = guard();
    let b = Book {
        open_positions: limits().max_open_positions,
        ..book()
    };
    assert_eq!(g.check(&prop("1"), &b), Err(Rejection::MaxOpenPositions));
}

#[test]
fn breach_one_position_per_symbol() {
    let mut g = guard();
    let b = Book {
        symbol_busy: true,
        ..book()
    };
    assert_eq!(g.check(&prop("1"), &b), Err(Rejection::SymbolBusy));
}

#[test]
fn breach_gross_exposure() {
    let mut g = guard();
    // 3× of ₹10 L = ₹30 L; ₹29 99 500 used leaves less than one share.
    let b = Book {
        gross_exposure: Price(29_99_500_00),
        ..book()
    };
    assert_eq!(g.check(&prop("1"), &b), Err(Rejection::Exposure));
    // Partly used: size shrinks to fit.
    let b = Book {
        gross_exposure: Price(29_00_000_00),
        ..book()
    };
    assert_eq!(g.check(&prop("1"), &b).unwrap().qty, 100);
}

#[test]
fn breach_max_order_qty_caps_size() {
    let mut g = guard();
    // ₹0.10 stop on a ₹100 stock would size to 50 000 shares.
    let p = EntryProposal {
        entry: Price(100_00),
        stop: Price(99_90),
        ltp: Some(Price(100_00)),
        ..prop("1")
    };
    assert_eq!(g.check(&p, &book()).unwrap().qty, limits().max_order_qty);
}

#[test]
fn breach_min_price() {
    let mut g = guard();
    let p = EntryProposal {
        entry: Price(49_95),
        stop: Price(49_00),
        ltp: Some(Price(49_95)),
        ..prop("1")
    };
    assert_eq!(g.check(&p, &book()), Err(Rejection::MinPrice));
}

#[test]
fn breach_stop_on_wrong_side_is_rejected() {
    let mut g = guard();
    let p = EntryProposal {
        stop: Price(1010_00),
        ..prop("1")
    };
    assert!(matches!(g.check(&p, &book()), Err(Rejection::BadSignal(_))));
    let p = EntryProposal {
        side: Side::Sell,
        stop: Price(990_00),
        ..prop("1")
    };
    assert!(matches!(g.check(&p, &book()), Err(Rejection::BadSignal(_))));
}

#[test]
fn breach_risk_too_wide_for_one_share() {
    let mut g = guard();
    // ₹6000 risk on one share exceeds the ₹5000 budget.
    let p = EntryProposal {
        entry: Price(10_000_00),
        stop: Price(4_000_00),
        ltp: Some(Price(10_000_00)),
        ..prop("1")
    };
    assert_eq!(g.check(&p, &book()), Err(Rejection::SizeZero));
}

#[test]
fn breach_circuit_band() {
    let mut g = guard();
    let p = EntryProposal {
        band: Some(PriceBand {
            lower: Price(900_00),
            upper: Price(1000_00),
        }),
        ..prop("1")
    };
    assert!(matches!(g.check(&p, &book()), Err(Rejection::PriceBand(_))));
}

#[test]
fn breach_entry_far_from_ltp() {
    let mut g = guard();
    let p = EntryProposal {
        ltp: Some(Price(1020_00)),
        ..prop("1")
    };
    assert!(matches!(g.check(&p, &book()), Err(Rejection::PriceBand(_))));
}

#[test]
fn breach_duplicate_signal() {
    let mut g = guard();
    g.check(&prop("1"), &book()).unwrap();
    let again = EntryProposal {
        now: at(10, 2),
        ..prop("1")
    };
    assert_eq!(g.check(&again, &book()), Err(Rejection::Duplicate));
    // Other side, other strategy, or after the window: not duplicates.
    let short = EntryProposal {
        side: Side::Sell,
        stop: Price(1010_00),
        ..again.clone()
    };
    assert!(g.check(&short, &book()).is_ok());
    let later = EntryProposal {
        now: at(10, 6),
        ..prop("1")
    };
    assert!(g.check(&later, &book()).is_ok());
}

#[test]
fn breach_consecutive_losses_cool_down() {
    let mut g = guard();
    let n = limits().max_consecutive_losses;
    for i in 0..n {
        g.on_trade_closed(at(10, i), Price(-100));
    }
    let p = EntryProposal {
        now: at(10, 10),
        ..prop("1")
    };
    assert!(matches!(g.check(&p, &book()), Err(Rejection::Cooldown { .. })));
    assert!(!g.entries_open(at(10, 10)));
    let after = at(10, n - 1) + TimeDelta::minutes(limits().consecutive_loss_cooldown_min);
    let p = EntryProposal {
        now: after,
        ..prop("1")
    };
    assert!(g.check(&p, &book()).is_ok());
    // A win resets the streak.
    let mut g = guard();
    for i in 0..n - 1 {
        g.on_trade_closed(at(10, i), Price(-100));
    }
    g.on_trade_closed(at(10, 20), Price(100));
    g.on_trade_closed(at(10, 21), Price(-100));
    assert!(g.check(&prop("1"), &book()).is_ok());
}

#[test]
fn state_survives_a_restart() {
    let mut g = guard();
    g.check(&prop("1"), &book()).unwrap();
    g.on_trade_closed(at(10, 1), Price(-100));
    g.on_pnl(Price(-30_000_00));
    let saved = g.state();
    let mut g2 = RiskGuard::restore(limits(), saved.clone());
    assert_eq!(g2.state(), saved);
    // begin_day on the same day keeps everything.
    g2.begin_day(day(), Price(1));
    assert_eq!(g2.capital(), CAPITAL);
    assert_eq!(g2.check(&prop("2"), &book()), Err(Rejection::DailyLossHit));
}

#[test]
fn fixed_capital_overrides_funds() {
    let l = RiskLimits {
        capital_base_inr: 200_000.0,
        ..limits()
    };
    let mut g = RiskGuard::new(l);
    g.begin_day(day(), CAPITAL);
    assert_eq!(g.capital(), Price(2_00_000_00));
    // Zero funds and no fixed capital: nothing can be sized.
    let mut g = RiskGuard::new(limits());
    g.begin_day(day(), Price::ZERO);
    assert_eq!(g.check(&prop("1"), &book()), Err(Rejection::NoCapital));
}
