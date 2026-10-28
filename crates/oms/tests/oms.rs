//! OMS behaviour against the paper broker with injected failures.
//!
//! Amounts are written rupees_paise: `1000_00` is ₹1000.00.

#![allow(clippy::inconsistent_digit_grouping)]

mod common;

use std::sync::Arc;

use common::*;
use saarthi_broker::{Broker, OrderKind, OrderRequest, OrderStatus, Product};
use saarthi_core::{Exchange, Price, Side};
use saarthi_oms::{Oms, OmsEvent, Role, TradeState};
use saarthi_risk::OrderThrottle;
use saarthi_store::Db;

fn stop_of(r: &Rig, id: &str) -> Option<saarthi_oms::OrderRec> {
    let t = r.oms.trade(id)?;
    t.orders
        .iter()
        .filter_map(|c| r.oms.order(c))
        .rfind(|o| o.role == Role::Stop)
        .cloned()
}

#[tokio::test]
async fn entry_fill_places_broker_side_stop_then_stop_closes() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    let id = r.oms.open(long("1", 50), r.now).await.unwrap();
    assert_eq!(r.oms.trade(&id).unwrap().state, TradeState::Pending);
    r.tick("1", 1001_00).await;
    let t = r.oms.trade(&id).unwrap();
    assert_eq!(t.state, TradeState::Open);
    assert_eq!(t.entry_qty, 50);
    assert_eq!(t.avg_entry(), Some(Price(1001_00)));
    let s = stop_of(&r, &id).unwrap();
    assert_eq!(s.req.kind, OrderKind::StopLossMarket { trigger: Price(990_00) });
    assert_eq!((s.req.side, s.req.qty), (Side::Sell, 50));
    assert!(s.broker_id.is_some());
    // The stop rests at the broker.
    let book = r.broker.order_book().await.unwrap();
    assert_eq!(book.iter().filter(|o| o.status == OrderStatus::Open).count(), 1);

    r.tick("1", 995_00).await;
    r.tick("1", 989_00).await;
    let t = r.oms.trade(&id).unwrap();
    assert_eq!(t.state, TradeState::Closed);
    assert_eq!(t.exit_reason.as_deref(), Some("stop"));
    assert_eq!(t.realized(), Price((989_00 - 1001_00) * 50));
    assert!(t.net(None) < t.realized(), "charges are deducted");
    let ev = r.oms.take_events();
    assert!(ev.iter().any(|(_, e)| matches!(e, OmsEvent::Closed { .. })));
    assert!(r.oms.broker_open_positions().await.unwrap().is_empty());
}

#[tokio::test]
async fn short_trade_stop_is_a_buy_above() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    let id = r.oms.open(short("1", 10), r.now).await.unwrap();
    r.tick("1", 1000_00).await;
    let s = stop_of(&r, &id).unwrap();
    assert_eq!(
        (s.req.side, s.req.kind),
        (
            Side::Buy,
            OrderKind::StopLossMarket {
                trigger: Price(1010_00)
            }
        )
    );
    r.tick("1", 1011_00).await;
    assert_eq!(r.oms.trade(&id).unwrap().state, TradeState::Closed);
}

#[tokio::test]
async fn partial_fills_resize_the_stop() {
    let mut r = Rig::new(Some(4));
    r.tick("1", 1000_00).await;
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    r.tick("1", 1000_00).await;
    assert_eq!(r.oms.trade(&id).unwrap().entry_qty, 4);
    assert_eq!(stop_of(&r, &id).unwrap().req.qty, 4, "stop covers what is filled");
    r.tick("1", 1000_00).await;
    assert_eq!(stop_of(&r, &id).unwrap().req.qty, 8);
    r.tick("1", 1000_00).await;
    assert_eq!(r.oms.trade(&id).unwrap().entry_qty, 10);
    assert_eq!(stop_of(&r, &id).unwrap().req.qty, 10);
    // The broker agrees.
    let book = r.broker.order_book().await.unwrap();
    let stop = book.iter().find(|o| o.status == OrderStatus::Open).unwrap();
    assert_eq!(stop.qty, 10);
    // Stop fills in pieces too.
    for _ in 0..3 {
        r.tick("1", 989_00).await;
    }
    let t = r.oms.trade(&id).unwrap();
    assert_eq!((t.state, t.exit_qty), (TradeState::Closed, 10));
}

#[tokio::test]
async fn rejected_entry_cancels_the_trade() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    r.broker.fail_next_place(Fault::Reject);
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    let t = r.oms.trade(&id).unwrap();
    assert_eq!(t.state, TradeState::Cancelled);
    let ev = r.oms.take_events();
    assert!(
        ev.iter().any(
            |(_, e)| matches!(e, OmsEvent::EntryRejected { reason, .. } if reason.contains("insufficient margin"))
        )
    );
    assert!(!r.oms.symbol_busy("1"));
}

#[tokio::test]
async fn stop_rejected_means_exit_at_market() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    // The entry fills on a tick already below the stop: the exchange rejects
    // the SL-M, and the OMS must not leave the position naked.
    r.tick("1", 989_00).await;
    let t = r.oms.trade(&id).unwrap();
    assert_eq!(t.state, TradeState::Exiting);
    assert_eq!(t.exit_reason.as_deref(), Some("stop_rejected"));
    r.tick("1", 988_00).await;
    assert_eq!(r.oms.trade(&id).unwrap().state, TradeState::Closed);
    assert!(r.oms.broker_open_positions().await.unwrap().is_empty());
    let ev = r.oms.take_events();
    assert!(ev.iter().any(|(_, e)| matches!(e, OmsEvent::Alert { .. })));
}

#[tokio::test]
async fn target_cancels_stop_then_exits() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    r.tick("1", 1005_00).await;
    let hit = r.oms.on_ltp("1", Price(1020_00), r.now).await.unwrap();
    assert_eq!(hit, vec![id.clone()]);
    assert_eq!(stop_of(&r, &id).unwrap().status, OrderStatus::Cancelled);
    r.tick("1", 1020_50).await;
    let t = r.oms.trade(&id).unwrap();
    assert_eq!(
        (t.state, t.exit_reason.as_deref()),
        (TradeState::Closed, Some("target"))
    );
    assert_eq!(t.avg_exit(), Some(Price(1020_50)));
    // Exactly one exit order: the stop never also filled.
    let sells: u32 = r
        .broker
        .order_book()
        .await
        .unwrap()
        .iter()
        .filter(|o| o.side == Side::Sell)
        .map(|o| o.filled_qty)
        .sum();
    assert_eq!(sells, 10);
}

#[tokio::test]
async fn square_off_retries_until_flat() {
    let mut r = Rig::new(None);
    for tok in ["1", "2", "3"] {
        r.tick(tok, 1000_00).await;
    }
    let a = r.oms.open(long("1", 10), r.now).await.unwrap();
    let b = r.oms.open(short("2", 10), r.now).await.unwrap();
    for tok in ["1", "2"] {
        r.tick(tok, 1000_00).await;
    }
    // A third trade whose entry never fills before square-off.
    let c = r.oms.open(long("3", 10), r.now).await.unwrap();
    // The first two exit orders are rejected by the broker.
    r.broker.fail_next_place(Fault::Reject);
    r.broker.fail_next_place(Fault::Reject);
    r.oms.flatten_all("square_off", r.now).await.unwrap();
    for _ in 0..4 {
        for tok in ["1", "2", "3"] {
            r.tick(tok, 1000_00).await;
        }
    }
    for id in [&a, &b] {
        let t = r.oms.trade(id).unwrap();
        assert_eq!(t.state, TradeState::Closed, "{t:?}");
        assert_eq!(t.exit_reason.as_deref(), Some("square_off"));
    }
    assert!(r.oms.trade(&c).unwrap().state.is_done());
    assert!(r.oms.broker_open_positions().await.unwrap().is_empty());
    assert_eq!(r.oms.live_trades().count(), 0);
    let book = r.broker.order_book().await.unwrap();
    assert!(book.iter().all(|o| o.status.is_terminal()), "nothing left working");
}

#[tokio::test]
async fn lost_reply_is_found_by_client_id() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    r.broker.fail_next_place(Fault::LoseReply);
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    let t = r.oms.trade(&id).unwrap();
    let e = r.oms.order(&t.orders[0]).unwrap();
    assert_eq!((e.status, e.broker_id.is_none()), (OrderStatus::Sent, true));
    r.tick("1", 1000_00).await;
    let t = r.oms.trade(&id).unwrap();
    assert_eq!(t.state, TradeState::Open, "adopted from the broker's book");
    assert!(r.oms.order(&t.orders[0]).unwrap().broker_id.is_some());
}

#[tokio::test]
async fn dropped_order_is_declared_lost_after_grace() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    r.broker.fail_next_place(Fault::Drop);
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    for _ in 0..20 {
        r.tick("1", 1000_00).await;
    }
    assert_eq!(r.oms.trade(&id).unwrap().state, TradeState::Cancelled);
}

#[tokio::test]
async fn reconcile_exits_orphans_and_flags_severe() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    r.tick("1", 1000_00).await;
    // Someone buys 5 more directly at the broker.
    r.broker
        .paper
        .place(OrderRequest {
            client_order_id: "manual".into(),
            symbol: symbol_of("1"),
            exchange: Exchange::Nse,
            token: "1".into(),
            side: Side::Buy,
            qty: 5,
            kind: OrderKind::Market,
            product: Product::Intraday,
            tag: "manual".into(),
        })
        .await
        .unwrap();
    r.tick("1", 1000_00).await;
    let first = r.oms.reconcile(r.now).await.unwrap();
    assert!(first.mismatches.is_empty(), "one sighting is not enough");
    let second = r.oms.reconcile(r.now).await.unwrap();
    assert!(second.severe, "{second:?}");
    r.tick("1", 1000_00).await;
    let positions = r.broker.positions().await.unwrap();
    assert_eq!(positions[0].net_qty, 10, "the orphan 5 were sold");
    assert_eq!(
        r.oms.trade(&id).unwrap().state,
        TradeState::Open,
        "our trade is untouched"
    );
    let third = r.oms.reconcile(r.now).await.unwrap();
    assert_eq!(third, Default::default());
}

#[tokio::test]
async fn reconcile_writes_off_positions_closed_at_the_broker() {
    let mut r = Rig::new(None);
    r.tick("1", 1000_00).await;
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    r.tick("1", 1000_00).await;
    // The broker's own square-off sells the position.
    r.broker
        .paper
        .place(OrderRequest {
            client_order_id: "rms".into(),
            symbol: symbol_of("1"),
            exchange: Exchange::Nse,
            token: "1".into(),
            side: Side::Sell,
            qty: 10,
            kind: OrderKind::Market,
            product: Product::Intraday,
            tag: "rms".into(),
        })
        .await
        .unwrap();
    r.tick("1", 1000_00).await;
    r.oms.reconcile(r.now).await.unwrap();
    let rec = r.oms.reconcile(r.now).await.unwrap();
    assert!(!rec.severe);
    assert_eq!(rec.mismatches.len(), 1);
    r.tick("1", 1000_00).await;
    let t = r.oms.trade(&id).unwrap();
    assert_eq!(t.state, TradeState::Closed);
    assert_eq!(t.exit_reason.as_deref(), Some("closed_at_broker"));
    // Its stop was cancelled rather than left to open a short.
    assert_eq!(stop_of(&r, &id).unwrap().status, OrderStatus::Cancelled);
}

#[tokio::test]
async fn restart_rebuilds_state_and_rearms_a_missing_stop() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("saarthi.db");
    let paper = saarthi_broker::Paper::open(paper_cfg(Some(dir.path().join("paper.json")), None)).unwrap();
    let broker = Arc::new(Faulty::new(paper));
    let mk =
        |b: Arc<Faulty>| Oms::new(b, Db::open(&db_path).unwrap(), OrderThrottle::new(50), oms_cfg(), day()).unwrap();
    let mut r = Rig {
        broker: broker.clone(),
        oms: mk(broker.clone()),
        now: t0(),
    };
    r.tick("1", 1000_00).await;
    let id = r.oms.open(long("1", 10), r.now).await.unwrap();
    r.tick("1", 1000_00).await;
    let stop_id = stop_of(&r, &id).unwrap().broker_id.unwrap();
    drop(r.oms);
    // While the engine is down the stop gets cancelled at the broker.
    broker.paper.cancel(&stop_id).await.unwrap();
    let mut oms = mk(broker.clone());
    assert_eq!(oms.trade(&id).unwrap().state, TradeState::Open);
    oms.recover(r.now).await.unwrap();
    let t = oms.trade(&id).unwrap().clone();
    let stops: Vec<_> = t
        .orders
        .iter()
        .filter_map(|c| oms.order(c))
        .filter(|o| o.role == Role::Stop)
        .collect();
    assert_eq!(stops.len(), 2);
    assert_eq!(stops[1].status, OrderStatus::Open, "re-armed at the broker");
    // The next trade id continues the day's numbering.
    broker.paper.on_tick(&tick("2", r.now, 1000_00)).unwrap();
    let id2 = oms.open(long("2", 10), r.now).await.unwrap();
    assert_ne!(id, id2);
    assert!(id2.ends_with("-0002"), "{id2}");
}
