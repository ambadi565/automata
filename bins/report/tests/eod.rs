//! The EOD report from a store with two closed trades and an incident.
#![allow(clippy::inconsistent_digit_grouping)]

use chrono::{NaiveDate, NaiveTime};
use engine::DaySummary;
use saarthi_core::{Exchange, MarketClock, Price, Side, Symbol, Ts};
use saarthi_oms::{Trade, TradeState};
use saarthi_store::Db;

fn day() -> NaiveDate {
    "2026-10-28".parse().unwrap()
}

fn at(h: u32, m: u32) -> Ts {
    MarketClock::at(day(), NaiveTime::from_hms_opt(h, m, 0).unwrap())
}

fn trade(id: &str, strategy: &str, side: Side, entry: i64, exit: i64, opened: Ts, closed: Ts) -> Trade {
    Trade {
        id: id.into(),
        day: day(),
        strategy: strategy.into(),
        token: "2885".into(),
        symbol: Symbol::new("RELIANCE-EQ"),
        exchange: Exchange::Nse,
        side,
        planned_qty: 10,
        signal_px: Price(entry - side.sign() * 10),
        signal_ts: opened,
        stop: Price(entry - side.sign() * 1_000),
        target: None,
        planned_risk: Price(10_000),
        reason: "test".into(),
        state: TradeState::Closed,
        entry_qty: 10,
        entry_value: entry * 10,
        exit_qty: 10,
        exit_value: exit * 10,
        charges: Price(5_000),
        exit_reason: Some("square_off".into()),
        opened_at: Some(opened),
        closed_at: Some(closed),
        orders: vec![],
        exit_failures: 0,
    }
}

fn store(dir: &std::path::Path) -> Db {
    let mut db = Db::open(&dir.join("saarthi.db")).unwrap();
    let a = trade("T1", "orb", Side::Buy, 1000_00, 1010_00, at(10, 5), at(11, 0));
    let b = trade("T2", "vwap_revert", Side::Sell, 2000_00, 2004_00, at(12, 0), at(13, 30));
    let summary = DaySummary {
        date: day(),
        trades: 2,
        wins: 1,
        losses: 1,
        gross: Price(6_000),
        charges: Price(10_000),
        net: Price(-4_000),
        flat_after_square_off: true,
        ..DaySummary::default()
    };
    db.write(|w| {
        for t in [&a, &b] {
            w.put_trade(&t.id, t.day, &t.token, t.state.as_str(), t)?;
        }
        w.put_day(day(), &summary)?;
        w.event(
            &at(12, 1).to_rfc3339(),
            "engine_alert",
            &serde_json::json!({"severity": "critical", "message": "feed <down> 130 s"}),
        )
    })
    .unwrap();
    db
}

#[test]
fn eod_report_numbers_chart_and_escaping() {
    let dir = tempfile::tempdir().unwrap();
    let db = store(dir.path());
    std::fs::write(
        dir.path().join(format!("{}.md", day())),
        "# EOD\n## Result\nmeh\n## Tomorrow: watchlist seed\nTCS-EQ on results.\n",
    )
    .unwrap();
    let e = report::load_eod(&db, day(), dir.path()).unwrap();
    assert_eq!(e.trades.len(), 2);

    let per = report::per_strategy(&e.trades);
    // Long 1000 → 1010 ×10 = +100, minus ₹50 charges.
    assert_eq!(per["orb"].net, Price(50_00));
    assert_eq!(per["orb"].avg_slippage, Price(10));
    // Short 2000 → 2004 ×10 = −40, minus ₹50.
    assert_eq!(per["vwap_revert"].net, Price(-90_00));

    let pts = report::equity_points(&e.trades);
    assert_eq!(pts.iter().map(|p| p.1).collect::<Vec<_>>(), vec![0, 50_00, -40_00]);

    let text = report::eod_text(&e);
    assert!(text.contains("Net ₹-40.00"), "{text}");
    assert!(text.contains("Incidents: 1"), "{text}");
    assert!(text.contains("Tomorrow: TCS-EQ on results."), "{text}");

    let html = report::eod_html(&e);
    assert!(html.contains("feed &lt;down&gt;"));
    assert!(!html.contains("feed <down>"));
    assert!(html.contains("<svg"));

    if report::fonts::ready() {
        let png = report::equity_png(&pts, "test").unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }
}

#[test]
fn a_day_without_a_session_still_reports() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("saarthi.db")).unwrap();
    let e = report::load_eod(&db, day(), dir.path()).unwrap();
    assert!(report::eod_text(&e).contains("No session recorded"));
    assert!(report::eod_html(&e).contains("No closed trades"));
}

#[test]
fn hourly_text_and_latest_note() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(format!("{}-hourly.md", day())),
        "\n\n<!-- 10:15 hourly -->\nold note\n\n<!-- 11:15 hourly -->\n**11:15** Trend intact; ORB working.\n",
    )
    .unwrap();
    let note = report::latest_note(dir.path(), day()).unwrap();
    assert_eq!(note, "**11:15** Trend intact; ORB working.");
    let state = serde_json::json!({"phase": "Open", "feed_ok": true, "halted": null, "plan_version": 3,
        "plan_is_default": false});
    let pnl = serde_json::json!({"day_pnl": 12345, "trades": 2, "wins": 1, "losses": 1, "loss_limit_used": 0.1});
    let pos = serde_json::json!({"positions": [{"symbol": "TCS-EQ", "side": "long", "qty": 5,
        "avg_entry": 300000, "stop": 297000, "net": 1500}]});
    let t = report::hourly_text(at(11, 15), &state, &pnl, &pos, Some(&note));
    assert!(t.contains("11:15 IST") && t.contains("Day P&L ₹123.45") && t.contains("TCS-EQ long 5 @ ₹3000.00"));
    assert!(t.contains("plan v3") && t.contains("ORB working"));
}
