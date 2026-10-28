//! `kill -9` mid-trade, restart, state intact:
//! `cargo test -p saarthi-oms -- crash_recovery`.
//!
//! The test re-runs its own binary as a child process that trades against a
//! file-backed paper broker and SQLite store, then SIGKILLs it at a chosen
//! moment (no destructors, no flush, no goodbye) and recovers in the parent.
//!
//! Amounts are written rupees_paise: `1000_00` is ₹1000.00.

#![allow(clippy::inconsistent_digit_grouping)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::TimeDelta;
use common::*;
use saarthi_broker::{Broker, OrderStatus, Paper};
use saarthi_oms::{Oms, Role, TradeState};
use saarthi_risk::OrderThrottle;
use saarthi_store::Db;

const CHILD: &str = "SAARTHI_CRASH_CHILD";
const DIR: &str = "SAARTHI_CRASH_DIR";

fn open(dir: &Path) -> (Arc<Faulty>, Oms<Faulty>) {
    let paper = Paper::open(paper_cfg(Some(dir.join("paper.json")), None)).unwrap();
    let broker = Arc::new(Faulty::new(paper));
    let oms = Oms::new(
        broker.clone(),
        Db::open(&dir.join("saarthi.db")).unwrap(),
        OrderThrottle::new(50),
        oms_cfg(),
        day(),
    )
    .unwrap();
    (broker, oms)
}

/// Runs in the child: trade up to the requested point, signal, then wait to
/// be killed.
async fn child(scenario: &str, dir: PathBuf) {
    let (broker, mut oms) = open(&dir);
    let mut now = t0();
    let mut step = |b: &Faulty, tok: &str, px: i64| {
        now += TimeDelta::seconds(1);
        b.paper.on_tick(&tick(tok, now, px)).unwrap();
        now
    };
    match scenario {
        "mid_trade" => {
            // Trade 1: filled and protected. Trade 2: entry sent, not filled.
            let n = step(&broker, "1", 1000_00);
            oms.open(long("1", 10), n).await.unwrap();
            let n = step(&broker, "1", 1001_00);
            oms.sync(n).await.unwrap();
            let n = step(&broker, "2", 1000_00);
            oms.open(short("2", 7), n).await.unwrap();
        }
        "before_send" => {
            // The entry is written to the store, then the process dies while
            // the request is in flight.
            let n = step(&broker, "1", 1000_00);
            broker.fail_next_place(Fault::Hang);
            std::fs::write(dir.join("ready"), b"").unwrap();
            oms.open(long("1", 10), n).await.unwrap();
            unreachable!("place never returns");
        }
        other => panic!("unknown scenario {other}"),
    }
    std::fs::write(dir.join("ready"), b"").unwrap();
    std::future::pending::<()>().await;
}

fn spawn_child(scenario: &str, dir: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args([
            format!("crash_recovery_{scenario}"),
            "--exact".into(),
            "--nocapture".into(),
        ])
        .env(CHILD, scenario)
        .env(DIR, dir)
        .spawn()
        .unwrap()
}

fn kill_when_ready(mut c: Child, dir: &Path) {
    let start = Instant::now();
    while !dir.join("ready").exists() {
        if let Some(status) = c.try_wait().unwrap() {
            panic!("child exited early: {status}");
        }
        assert!(start.elapsed() < Duration::from_secs(60), "child never got ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    // A moment for the hanging request to be in flight.
    std::thread::sleep(Duration::from_millis(100));
    c.kill().unwrap(); // SIGKILL
    let status = c.wait().unwrap();
    assert!(!status.success(), "child must die by the signal");
}

/// Returns true when running as the child (the test body is skipped).
async fn as_child() -> bool {
    match (std::env::var(CHILD), std::env::var(DIR)) {
        (Ok(s), Ok(d)) => {
            child(&s, d.into()).await;
            true
        }
        _ => false,
    }
}

#[tokio::test]
async fn crash_recovery_mid_trade() {
    if as_child().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    kill_when_ready(spawn_child("mid_trade", dir), dir);

    let (broker, mut oms) = open(dir);
    let live: Vec<_> = oms.live_trades().cloned().collect();
    assert_eq!(live.len(), 2, "both trades come back from SQLite");
    let mut now = t0() + TimeDelta::minutes(5);
    oms.recover(now).await.unwrap();
    let t1 = live.iter().find(|t| t.token == "1").unwrap().id.clone();
    let t2 = live.iter().find(|t| t.token == "2").unwrap().id.clone();
    let a = oms.trade(&t1).unwrap();
    assert_eq!((a.state, a.open_qty()), (TradeState::Open, 10));
    // Its stop is still the one placed before the crash, working at the broker.
    let stops: Vec<_> = a
        .orders
        .iter()
        .filter_map(|c| oms.order(c))
        .filter(|o| o.role == Role::Stop)
        .collect();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].status, OrderStatus::Open);
    assert_eq!(oms.trade(&t2).unwrap().state, TradeState::Pending);

    // Life goes on: trade 2 fills and gets its stop, then both stop out.
    for (tok, px) in [("2", 1000_00), ("1", 980_00), ("2", 1020_00)] {
        now += TimeDelta::seconds(1);
        broker.paper.on_tick(&tick(tok, now, px)).unwrap();
        oms.sync(now).await.unwrap();
    }
    for id in [&t1, &t2] {
        let t = oms.trade(id).unwrap();
        assert_eq!(
            (t.state, t.exit_reason.as_deref()),
            (TradeState::Closed, Some("stop")),
            "{t:?}"
        );
    }
    assert!(oms.broker_open_positions().await.unwrap().is_empty());
    let rec = oms.reconcile(now).await.unwrap();
    assert!(rec.mismatches.is_empty());
}

#[tokio::test]
async fn crash_recovery_before_send() {
    if as_child().await {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    kill_when_ready(spawn_child("before_send", dir), dir);

    let (broker, mut oms) = open(dir);
    let live: Vec<_> = oms.live_trades().cloned().collect();
    assert_eq!(live.len(), 1, "the order was written before it was sent");
    let id = live[0].id.clone();
    assert!(
        broker.order_book().await.unwrap().is_empty(),
        "and never reached the broker"
    );
    // After the grace period the OMS gives up on it instead of guessing.
    oms.recover(t0() + TimeDelta::minutes(1)).await.unwrap();
    assert_eq!(oms.trade(&id).unwrap().state, TradeState::Cancelled);
    assert!(!oms.symbol_busy("1"));
}
