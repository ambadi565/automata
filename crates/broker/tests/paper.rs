//! The paper broker against synthetic ticks.
//!
//! Amounts are written rupees_paise: `1000_00` is ₹1000.00.

#![allow(clippy::inconsistent_digit_grouping)]

use chrono::{NaiveDate, NaiveTime, TimeDelta};
use saarthi_broker::backtest::{CostModel, ExecutionCosts, Slippage};
use saarthi_broker::{Broker, Modify, OrderKind, OrderRequest, OrderStatus, Paper, PaperConfig, Product};
use saarthi_core::{Exchange, MarketClock, Price, Side, Symbol, Tick, Ts};

fn costs() -> CostModel {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/costs.toml");
    ExecutionCosts::load(std::path::Path::new(p)).unwrap().charges
}

fn cfg(state: Option<std::path::PathBuf>) -> PaperConfig {
    PaperConfig {
        capital: Price(1_00_000_00),
        costs: costs(),
        slippage: Slippage {
            bps: 0.0,
            ticks: 1,
            tick_size_inr: 0.05,
            limit_through_ticks: 1,
        },
        max_fill_qty_per_tick: None,
        state_path: state,
    }
}

fn t0() -> Ts {
    MarketClock::at(
        NaiveDate::from_ymd_opt(2026, 10, 28).unwrap(),
        NaiveTime::from_hms_opt(9, 30, 0).unwrap(),
    )
}

fn tick(sec: i64, ltp: i64) -> Tick {
    Tick {
        token: "2885".into(),
        exchange: Exchange::Nse,
        ts: t0() + TimeDelta::seconds(sec),
        ltp: Price(ltp),
        last_qty: Some(10),
        day_volume: None,
        avg_price: None,
    }
}

fn req(id: &str, side: Side, qty: u32, kind: OrderKind) -> OrderRequest {
    OrderRequest {
        client_order_id: id.into(),
        symbol: Symbol::new("RELIANCE-EQ"),
        exchange: Exchange::Nse,
        token: "2885".into(),
        side,
        qty,
        kind,
        product: Product::Intraday,
        tag: "test".into(),
    }
}

#[tokio::test]
async fn market_order_fills_on_next_tick_with_slippage() {
    let p = Paper::open(cfg(None)).unwrap();
    p.on_tick(&tick(0, 100_000)).unwrap();
    let id = p.place(req("e1", Side::Buy, 10, OrderKind::Market)).await.unwrap();
    assert_eq!(p.order_book().await.unwrap()[0].status, OrderStatus::Open);
    let fills = p.on_tick(&tick(1, 100_100)).unwrap();
    assert_eq!(fills.len(), 1);
    // Next tick's LTP plus one ₹0.05 tick, never the price seen before the order.
    assert_eq!(fills[0].price, Price(100_105));
    let ob = p.order_book().await.unwrap();
    assert_eq!(ob[0].id, id);
    assert_eq!(ob[0].status, OrderStatus::Filled);
    assert_eq!(ob[0].avg_price, Some(Price(100_105)));
    let pos = p.positions().await.unwrap();
    assert_eq!(pos[0].net_qty, 10);
    assert!(fills[0].charges.0 > 0);
}

#[tokio::test]
async fn stop_triggers_through_and_gaps_fill_worse() {
    let p = Paper::open(cfg(None)).unwrap();
    p.on_tick(&tick(0, 100_000)).unwrap();
    p.place(req("e1", Side::Buy, 10, OrderKind::Market)).await.unwrap();
    p.on_tick(&tick(1, 100_000)).unwrap();
    p.place(req(
        "s1",
        Side::Sell,
        10,
        OrderKind::StopLossMarket { trigger: Price(99_000) },
    ))
    .await
    .unwrap();
    assert!(p.on_tick(&tick(2, 99_500)).unwrap().is_empty());
    // Gap from 995 to 980: fills at 980 minus a tick, not at the 990 trigger.
    let f = p.on_tick(&tick(3, 98_000)).unwrap();
    assert_eq!(f[0].price, Price(97_995));
    assert_eq!(p.positions().await.unwrap()[0].net_qty, 0);
    let realized = p.positions().await.unwrap()[0].realized;
    assert_eq!(realized, Price((97_995 - 100_005) * 10));
}

#[tokio::test]
async fn stop_through_the_market_is_rejected() {
    let p = Paper::open(cfg(None)).unwrap();
    p.on_tick(&tick(0, 100_000)).unwrap();
    p.place(req(
        "s1",
        Side::Sell,
        10,
        OrderKind::StopLossMarket {
            trigger: Price(100_500),
        },
    ))
    .await
    .unwrap();
    let o = &p.order_book().await.unwrap()[0];
    assert_eq!(o.status, OrderStatus::Rejected);
    assert!(o.reject_reason.as_deref().unwrap().contains("not below LTP"));
}

#[tokio::test]
async fn resting_limit_needs_a_tick_through() {
    let p = Paper::open(cfg(None)).unwrap();
    p.on_tick(&tick(0, 100_000)).unwrap();
    p.place(req("l1", Side::Sell, 5, OrderKind::Limit { price: Price(101_000) }))
        .await
        .unwrap();
    assert!(p.on_tick(&tick(1, 100_500)).unwrap().is_empty());
    assert!(
        p.on_tick(&tick(2, 101_000)).unwrap().is_empty(),
        "a touch is not a fill"
    );
    let f = p.on_tick(&tick(3, 101_005)).unwrap();
    assert_eq!(f[0].price, Price(101_000));
}

#[tokio::test]
async fn partial_fills_and_cancel_of_remainder() {
    let mut c = cfg(None);
    c.max_fill_qty_per_tick = Some(4);
    let p = Paper::open(c).unwrap();
    p.on_tick(&tick(0, 100_000)).unwrap();
    let id = p.place(req("e1", Side::Buy, 10, OrderKind::Market)).await.unwrap();
    p.on_tick(&tick(1, 100_000)).unwrap();
    let o = p.order_book().await.unwrap()[0].clone();
    assert_eq!((o.status, o.filled_qty), (OrderStatus::PartiallyFilled, 4));
    p.cancel(&id).await.unwrap();
    p.on_tick(&tick(2, 100_000)).unwrap();
    let o = p.order_book().await.unwrap()[0].clone();
    assert_eq!((o.status, o.filled_qty), (OrderStatus::Cancelled, 4));
    assert_eq!(p.positions().await.unwrap()[0].net_qty, 4);
    assert!(p.cancel(&id).await.is_err(), "cannot cancel twice");
}

#[tokio::test]
async fn modify_rules() {
    let p = Paper::open(cfg(None)).unwrap();
    p.on_tick(&tick(0, 100_000)).unwrap();
    let id = p
        .place(req(
            "s1",
            Side::Sell,
            10,
            OrderKind::StopLossMarket { trigger: Price(99_000) },
        ))
        .await
        .unwrap();
    p.modify(
        &id,
        Modify {
            qty: Some(20),
            trigger: Some(Price(99_500)),
            ..Modify::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(p.order_book().await.unwrap()[0].qty, 20);
    // A limit price on a stop order is a mismatch.
    assert!(
        p.modify(
            &id,
            Modify {
                price: Some(Price(1)),
                ..Modify::default()
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn account_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("paper.json");
    {
        let p = Paper::open(cfg(Some(path.clone()))).unwrap();
        p.on_tick(&tick(0, 100_000)).unwrap();
        p.place(req("e1", Side::Buy, 10, OrderKind::Market)).await.unwrap();
        p.on_tick(&tick(1, 100_000)).unwrap();
        p.place(req(
            "s1",
            Side::Sell,
            10,
            OrderKind::StopLossMarket { trigger: Price(99_000) },
        ))
        .await
        .unwrap();
    }
    let p = Paper::open(cfg(Some(path))).unwrap();
    let ob = p.order_book().await.unwrap();
    assert_eq!(ob.len(), 2);
    assert_eq!(ob[1].status, OrderStatus::Open);
    assert_eq!(p.positions().await.unwrap()[0].net_qty, 10);
    // The stop still works after the restart.
    assert_eq!(p.on_tick(&tick(5, 98_900)).unwrap().len(), 1);
}

#[tokio::test]
async fn new_day_books_pnl_into_cash() {
    let p = Paper::open(cfg(None)).unwrap();
    p.on_tick(&tick(0, 100_000)).unwrap();
    p.place(req("e1", Side::Buy, 10, OrderKind::Market)).await.unwrap();
    p.on_tick(&tick(1, 100_000)).unwrap();
    p.place(req("x1", Side::Sell, 10, OrderKind::Market)).await.unwrap();
    p.on_tick(&tick(2, 101_000)).unwrap();
    let eq = p.funds().await.unwrap().net;
    let mut next = tick(0, 101_000);
    next.ts += TimeDelta::days(1);
    p.on_tick(&next).unwrap();
    assert_eq!(p.funds().await.unwrap().net, eq);
    assert!(p.order_book().await.unwrap().is_empty());
    assert_eq!(p.charges(), Price::ZERO);
}
