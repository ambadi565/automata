//! Fill rules of the simulated exchange.

use chrono::{DateTime, TimeDelta, Utc};
use saarthi_broker::backtest::{CostModel, ExecutionCosts, FillKind, SimExchange, Slippage};
use saarthi_broker::{Backtest, Broker, OrderKind, OrderRequest, OrderStatus, Product};
use saarthi_core::{Bar, Exchange, Price, Side, Symbol, Ts};

/// ₹1 lakh, in paise.
const CAPITAL: i64 = 10_000_000;

fn t(i: i64) -> Ts {
    // 2026-10-28 09:15 IST
    DateTime::<Utc>::from_timestamp(1_793_159_100, 0).unwrap() + TimeDelta::minutes(5 * i)
}

fn bar(i: i64, o: i64, h: i64, l: i64, c: i64) -> Bar {
    Bar {
        ts: t(i),
        open: Price(o),
        high: Price(h),
        low: Price(l),
        close: Price(c),
        volume: 1000,
    }
}

fn costs() -> CostModel {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/costs.toml");
    ExecutionCosts::load(std::path::Path::new(p)).unwrap().charges
}

fn no_slip() -> Slippage {
    Slippage {
        bps: 0.0,
        ticks: 0,
        tick_size_inr: 0.05,
        limit_through_ticks: 0,
    }
}

fn sim(slip: Slippage) -> SimExchange {
    SimExchange::new(costs(), slip, Price(CAPITAL))
}

fn req(side: Side, qty: u32) -> OrderRequest {
    OrderRequest {
        client_order_id: "c1".into(),
        symbol: Symbol::new("TEST-EQ"),
        exchange: Exchange::Nse,
        token: "1".into(),
        side,
        qty,
        kind: OrderKind::Market,
        product: Product::Intraday,
        tag: "bt".into(),
    }
}

#[test]
fn market_order_fills_at_next_bar_open_never_the_signal_bar() {
    let mut s = sim(Slippage::default());
    // Signal at the close of bar 0 (t(1)); the order is placed then.
    s.set_time(t(1));
    s.place(req(Side::Buy, 10)).unwrap();
    // Replaying bar 0 again (or any earlier bar) must not fill it.
    assert!(s.on_bar("1", &bar(0, 100_000, 100_500, 99_500, 100_200)).is_empty());
    let f = s.on_bar("1", &bar(1, 100_300, 100_600, 100_100, 100_400));
    assert_eq!(f.len(), 1);
    // Open ₹1003 + 2 bps (20 paise) + 1 tick (5 paise) = ₹1003.25
    assert_eq!(f[0].price, Price(100_325));
    assert_eq!(f[0].kind, FillKind::Market);
    assert!(f[0].charges.total().0 > 0);
    assert_eq!(s.position("1").qty, 10);
}

#[test]
fn bracket_stop_wins_when_both_legs_trade_in_one_bar() {
    let mut s = sim(no_slip());
    s.set_time(t(1));
    s.place_bracket(req(Side::Buy, 10), Price(99_000), Some(Price(101_000)))
        .unwrap();
    // Entry fills at 1000; same bar trades both 990 and 1010.
    let f = s.on_bar("1", &bar(1, 100_000, 101_500, 98_500, 100_000));
    assert_eq!(f.len(), 2);
    assert_eq!(f[1].kind, FillKind::Stop);
    assert_eq!(f[1].price, Price(99_000));
    assert_eq!(s.position("1").qty, 0);
    assert_eq!(s.position("1").realized, Price(-10_000));
    // The target was cancelled, nothing left working.
    assert_eq!(s.working_count("1"), 0);
}

#[test]
fn gap_through_stop_fills_at_the_open() {
    let mut s = sim(no_slip());
    s.set_time(t(1));
    s.place_bracket(req(Side::Buy, 10), Price(99_000), Some(Price(105_000)))
        .unwrap();
    s.on_bar("1", &bar(1, 100_000, 100_200, 99_800, 100_000));
    // Next bar opens well below the stop.
    let f = s.on_bar("1", &bar(2, 97_000, 97_500, 96_500, 97_200));
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].price, Price(97_000), "gap fills at the open, not the trigger");
}

#[test]
fn target_needs_to_trade_through_by_configured_ticks() {
    let mut slip = no_slip();
    slip.limit_through_ticks = 1;
    let mut s = sim(slip);
    s.set_time(t(1));
    s.place_bracket(req(Side::Sell, 10), Price(101_000), Some(Price(99_000)))
        .unwrap();
    s.on_bar("1", &bar(1, 100_000, 100_200, 99_800, 100_000));
    // Touches 990.00 exactly: not filled.
    assert!(s.on_bar("1", &bar(2, 99_500, 99_600, 99_000, 99_200)).is_empty());
    // Trades 989.95: filled at the limit.
    let f = s.on_bar("1", &bar(3, 99_200, 99_300, 98_995, 99_100));
    assert_eq!(f[0].kind, FillKind::Limit);
    assert_eq!(f[0].price, Price(99_000));
    assert_eq!(
        s.position("1").realized,
        Price(10_000),
        "short from 1000 covered at 990"
    );
}

#[test]
fn equity_is_capital_plus_pnl_minus_charges() {
    let mut s = sim(Slippage::default());
    s.set_time(t(1));
    s.place(req(Side::Buy, 100)).unwrap();
    s.on_bar("1", &bar(1, 100_000, 100_000, 100_000, 100_000));
    s.flatten("1", "sqoff").unwrap();
    s.on_bar("1", &bar(2, 101_000, 101_000, 101_000, 101_000));
    let pos = s.position("1");
    assert_eq!(pos.qty, 0);
    let charges = s.total_charges().total();
    assert_eq!(s.equity(), Price(CAPITAL) + pos.realized - charges);
    assert!(charges.0 > 0);
}

#[test]
fn rejects_what_it_cannot_simulate() {
    let mut s = sim(no_slip());
    assert!(s.place(req(Side::Buy, 0)).is_err());
    let mut r = req(Side::Buy, 1);
    r.product = Product::Delivery;
    assert!(s.place(r).is_err());
    assert!(
        s.place_bracket(req(Side::Buy, 1), Price(101_000), Some(Price(100_500)))
            .is_err()
    );
}

#[tokio::test]
async fn broker_trait_round_trip() {
    let bt = Backtest::new(sim(no_slip()));
    bt.login().await.unwrap();
    bt.set_time(t(1));
    let id = bt.place(req(Side::Buy, 5)).await.unwrap();
    bt.on_bar("1", &bar(1, 100_000, 100_000, 100_000, 100_000));
    let book = bt.order_book().await.unwrap();
    assert_eq!(book[0].id, id);
    assert_eq!(book[0].status, OrderStatus::Filled);
    let pos = bt.positions().await.unwrap();
    assert_eq!(pos[0].net_qty, 5);
    assert!(bt.cancel(&id).await.is_err(), "filled orders cannot be cancelled");
    let funds = bt.funds().await.unwrap();
    assert!(funds.net < Price(CAPITAL), "charges were paid");
}
