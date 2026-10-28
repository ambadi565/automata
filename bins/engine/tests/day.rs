//! Whole paper-trading days through the engine with synthetic ticks: an ORB
//! breakout is entered, protected by a broker-side stop and squared off by
//! 15:10; the daily loss stop, the kill switch and a mid-day restart each get
//! a day of their own.
//!
//! Amounts are written rupees_paise: `1000_00` is ₹1000.00.

#![allow(clippy::inconsistent_digit_grouping)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{NaiveDate, NaiveTime, TimeDelta};
use engine::alerts::MemoryAlerts;
use engine::{Engine, Parts};
use saarthi_bars::{BarConfig, IndicatorParams};
use saarthi_broker::backtest::ExecutionCosts;
use saarthi_broker::{Broker, Paper, PaperConfig};
use saarthi_core::{Bar, Exchange, HolidayCalendar, Instrument, MarketClock, Price, Symbol, Tick, Ts};
use saarthi_oms::{Severity, TradeState};
use saarthi_risk::{HaltReason, RiskLimits};
use saarthi_store::Db;
use saarthi_strategy::Direction;

const TOKEN: &str = "2885";

fn cfg(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config").join(name)
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
}

fn at(d: NaiveDate, h: u32, m: u32, s: u32) -> Ts {
    MarketClock::at(d, NaiveTime::from_hms_opt(h, m, s).unwrap())
}

fn instrument() -> Instrument {
    Instrument {
        token: TOKEN.into(),
        symbol: Symbol::new("RELIANCE-EQ"),
        name: "RELIANCE".into(),
        exchange: Exchange::Nse,
        instrument_type: String::new(),
        lot_size: 1,
        tick_size: Price(5),
        expiry: None,
        strike: None,
    }
}

/// Three quiet earlier sessions of 5m bars around ₹1000 for the indicators.
fn warmup() -> Vec<Bar> {
    let mut out = Vec::new();
    for back in [3, 2, 1] {
        let d = day() - TimeDelta::days(back);
        let mut ts = at(d, 9, 15, 0);
        for i in 0..75 {
            let o = 1000_00 + if i % 2 == 0 { 50 } else { -50 };
            out.push(Bar {
                ts,
                open: Price(o),
                high: Price(o + 100),
                low: Price(o - 100),
                close: Price(o),
                volume: if i % 2 == 0 { 2_500 } else { 3_500 },
            });
            ts += TimeDelta::minutes(5);
        }
    }
    out
}

struct Sim {
    paper: Paper,
    engine: Engine<Paper>,
    alerts: Arc<MemoryAlerts>,
    now: Ts,
    volume: u64,
    halt: PathBuf,
}

fn parts(paper: &Paper, db: Db, alerts: Arc<MemoryAlerts>, halt: PathBuf) -> Parts<Paper> {
    let limits = RiskLimits::load(&cfg("risk.toml")).unwrap();
    let costs = ExecutionCosts::load(&cfg("costs.toml")).unwrap();
    let clock = MarketClock::new(
        limits.session().unwrap(),
        HolidayCalendar::load(&cfg("holidays.toml")).unwrap(),
    );
    Parts {
        broker: Arc::new(paper.clone()),
        paper: Some(paper.clone()),
        db,
        limits: limits.clone(),
        costs: costs.charges,
        clock: clock.clone(),
        instruments: vec![instrument()],
        strategies: vec!["orb".into()],
        direction: Direction::Both,
        bars: BarConfig {
            session: clock.times,
            ..BarConfig::default()
        },
        indicators: IndicatorParams::default(),
        warmup: HashMap::from([(TOKEN.to_string(), warmup())]),
        alerts,
        halt_file: Some(halt),
        reconcile_every: TimeDelta::seconds(30),
        sync_every: TimeDelta::seconds(1),
        square_off_retry: TimeDelta::seconds(5),
        square_off_alert_after: 3,
        default_plan: saarthi_plan::load(&cfg("plan.default.json")).unwrap(),
        index_tokens: vec![("NIFTY".into(), "99926000".into())],
    }
}

fn paper_at(dir: &Path) -> Paper {
    let costs = ExecutionCosts::load(&cfg("costs.toml")).unwrap();
    Paper::open(PaperConfig {
        capital: Price(10_00_000_00),
        costs: costs.charges,
        slippage: costs.slippage,
        max_fill_qty_per_tick: None,
        state_path: Some(dir.join("paper.json")),
    })
    .unwrap()
}

impl Sim {
    async fn new(dir: &Path) -> Sim {
        let paper = paper_at(dir);
        let alerts = Arc::new(MemoryAlerts::default());
        let halt = dir.join("HALT");
        let db = Db::open(&dir.join("saarthi.db")).unwrap();
        let now = at(day(), 8, 59, 0);
        let engine = Engine::new(parts(&paper, db, alerts.clone(), halt.clone()), now)
            .await
            .unwrap();
        Sim {
            paper,
            engine,
            alerts,
            now,
            volume: 0,
            halt,
        }
    }

    /// Ticks every 10 s (with a timer call each) until `until`, at the price
    /// `px(now)` with `vol` shares per tick.
    async fn run(&mut self, until: Ts, px: impl Fn(Ts) -> i64, vol: u64) {
        let open = at(day(), 9, 15, 0);
        let close = at(day(), 15, 30, 0);
        while self.now < until {
            self.now += TimeDelta::seconds(10);
            if self.now >= open && self.now < close {
                self.volume += vol;
                let t = Tick {
                    token: TOKEN.into(),
                    exchange: Exchange::Nse,
                    ts: self.now,
                    ltp: Price(px(self.now)),
                    last_qty: Some(vol),
                    day_volume: Some(self.volume),
                    avg_price: None,
                };
                self.engine.on_tick(&t, self.now).await.unwrap();
            }
            self.engine.on_timer(self.now).await.unwrap();
        }
    }

    /// Opening range ₹998–1002 from 09:15, quiet until 10:00, then a
    /// high-volume breakout bar closing at ₹1006 at 10:05.
    async fn run_to_breakout(&mut self) {
        let wiggle = |t: Ts| {
            let s = (t - at(day(), 9, 15, 0)).num_seconds() / 10;
            [998_00, 1000_00, 1002_00, 1000_00][(s % 4) as usize]
        };
        self.run(at(day(), 9, 30, 0), wiggle, 100).await;
        self.run(at(day(), 10, 0, 0), |_| 1000_00, 100).await;
        self.run(
            at(day(), 10, 5, 0),
            |t| 1003_00 + (t - at(day(), 10, 0, 0)).num_seconds(),
            1_000,
        )
        .await;
        // The 10:05 bar closes on the next tick; the entry fills on the one after.
        self.run(at(day(), 10, 6, 0), |_| 1006_00, 100).await;
    }

    fn criticals(&self) -> Vec<String> {
        self.alerts
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| *s == Severity::Critical)
            .map(|(_, m)| m.clone())
            .collect()
    }
}

#[tokio::test]
async fn breakout_is_traded_protected_and_squared_off_by_1510() {
    let dir = tempfile::tempdir().unwrap();
    let mut sim = Sim::new(dir.path()).await;
    sim.run_to_breakout().await;

    let trades: Vec<_> = sim.engine.oms().trades().cloned().collect();
    assert_eq!(trades.len(), 1, "one ORB entry: {:?}", sim.engine.summary().rejected);
    let t = &trades[0];
    assert_eq!(t.state, TradeState::Open, "{t:#?} {:?}", sim.alerts.0.lock().unwrap());
    assert!(t.entry_qty > 0);
    // Sized by risk: (entry − stop) × qty ≤ 0.5 % of ₹10 L.
    assert!(t.planned_risk <= Price(5_000_00), "{t:?}");
    // Broker-side stop is resting.
    let book = sim.paper.order_book().await.unwrap();
    assert!(book.iter().any(|o| o.status == saarthi_broker::OrderStatus::Open));

    // Drift sideways to the close; nothing may be open after 15:10.
    sim.run(at(day(), 15, 12, 0), |_| 1007_00, 100).await;
    assert!(sim.paper.positions().await.unwrap().iter().all(|p| p.net_qty == 0));
    let t = sim.engine.oms().trade(&t.id).unwrap();
    assert_eq!(
        (t.state, t.exit_reason.as_deref()),
        (TradeState::Closed, Some("square_off"))
    );
    let s = sim.engine.summary().clone();
    assert!(s.flat_after_square_off);
    assert!(s.flat_confirmed_at.unwrap() <= at(day(), 15, 11, 0));
    assert_eq!((s.trades, s.restarts), (1, 0));
    assert!(s.charges.0 > 0);
    assert!(sim.criticals().is_empty(), "{:?}", sim.criticals());

    // The day is in the store for the exit check.
    sim.run(at(day(), 15, 36, 0), |_| 1007_00, 100).await;
    let db = Db::open(&dir.path().join("saarthi.db")).unwrap();
    let saved: engine::DaySummary = db.day(day()).unwrap().unwrap();
    assert!(saved.flat_after_square_off);
    assert_eq!(saved.trades, 1);
}

#[tokio::test]
async fn gap_through_the_stop_hits_daily_loss_flattens_and_halts() {
    let dir = tempfile::tempdir().unwrap();
    let mut sim = Sim::new(dir.path()).await;
    sim.run_to_breakout().await;
    assert_eq!(sim.engine.oms().live_trades().count(), 1);
    // A 5 % gap down: the stop fills far below its trigger.
    sim.run(at(day(), 10, 7, 0), |_| 955_00, 100).await;
    assert!(sim.engine.day_pnl().0 < -20_000_00, "{}", sim.engine.day_pnl());
    assert_eq!(sim.engine.risk().halted(), Some(&HaltReason::DailyLoss));
    assert_eq!(sim.engine.oms().live_trades().count(), 0);
    assert!(sim.criticals().iter().any(|m| m.contains("daily loss")));
    // A second breakout later in the day is refused.
    sim.run(at(day(), 10, 30, 0), |_| 955_00, 100).await;
    assert_eq!(sim.engine.oms().trades().count(), 1);
    sim.run(at(day(), 15, 15, 0), |_| 955_00, 100).await;
    assert!(sim.engine.summary().flat_after_square_off);
    assert!(sim.engine.summary().halted.is_some());
    // The strategist cannot add risk back after the loss.
    let p = plan_json(1, r#""global_risk_multiplier":1.0"#);
    let r = sim
        .engine
        .handle(
            saarthi_control::Request::SubmitPlan(p),
            sim.now - TimeDelta::minutes(10),
        )
        .await
        .unwrap();
    assert!(r["clamped"].to_string().contains("no adding risk"), "{r}");
    assert!((sim.engine.plan().plan.global_risk_multiplier - 0.5).abs() < 1e-9);
}

#[tokio::test]
async fn kill_switch_flattens_and_blocks_until_removed() {
    let dir = tempfile::tempdir().unwrap();
    let mut sim = Sim::new(dir.path()).await;
    sim.run_to_breakout().await;
    std::fs::write(&sim.halt, "telegram /halt").unwrap();
    sim.run(at(day(), 10, 7, 0), |_| 1006_00, 100).await;
    assert!(matches!(sim.engine.risk().halted(), Some(HaltReason::Manual(m)) if m == "telegram /halt"));
    assert_eq!(sim.engine.oms().live_trades().count(), 0);
    assert!(sim.paper.positions().await.unwrap().iter().all(|p| p.net_qty == 0));
    std::fs::remove_file(&sim.halt).unwrap();
    sim.run(at(day(), 10, 8, 0), |_| 1006_00, 100).await;
    assert!(sim.engine.risk().halted().is_none(), "resumed before 14:45");
}

#[tokio::test]
async fn restart_mid_trade_recovers_and_still_ends_flat() {
    let dir = tempfile::tempdir().unwrap();
    let mut sim = Sim::new(dir.path()).await;
    sim.run_to_breakout().await;
    let id = sim.engine.oms().live_trades().next().unwrap().id.clone();
    let now = sim.now;
    drop(sim);

    // A new process: same database, same paper account.
    let paper = paper_at(dir.path());
    let alerts = Arc::new(MemoryAlerts::default());
    let db = Db::open(&dir.path().join("saarthi.db")).unwrap();
    let engine = Engine::new(parts(&paper, db, alerts.clone(), dir.path().join("HALT")), now)
        .await
        .unwrap();
    assert_eq!(engine.summary().restarts, 1);
    assert_eq!(engine.oms().trade(&id).unwrap().state, TradeState::Open);
    let mut sim = Sim {
        paper,
        engine,
        alerts,
        now,
        volume: 1_000_000,
        halt: dir.path().join("HALT"),
    };
    sim.run(at(day(), 15, 15, 0), |_| 1007_00, 100).await;
    assert_eq!(sim.engine.oms().trade(&id).unwrap().state, TradeState::Closed);
    assert!(sim.engine.summary().flat_after_square_off);
    // The trade count survived the restart (no second ORB entry today).
    assert_eq!(sim.engine.risk().state().trades_today, 1);
}

fn plan_json(version: u64, body: &str) -> String {
    format!(
        r#"{{"version":{version},"valid_until":"{}T15:30:00+05:30","market_bias":"neutral","regime":"trend",{body}}}"#,
        day()
    )
}

#[tokio::test]
async fn submitted_plan_is_clamped_versioned_and_survives_restart() {
    use saarthi_control::Request;
    let dir = tempfile::tempdir().unwrap();
    let mut sim = Sim::new(dir.path()).await;
    let now = sim.now;
    assert!(sim.engine.plan().is_default);

    // Over-limit risk and an unknown symbol are clamped, not obeyed.
    let p = plan_json(
        1,
        r#""global_risk_multiplier":3.0,"watchlist":[
            {"symbol":"RELIANCE-EQ","strategies":["orb","martingale"],"max_risk_pct":9.0},
            {"symbol":"GME","strategies":["orb"],"max_risk_pct":0.1}]"#,
    );
    let r = sim.engine.handle(Request::SubmitPlan(p), now).await.unwrap();
    assert_eq!(r["version"], 1);
    let changes = r["clamped"].to_string();
    assert!(changes.contains("clamped to 1"), "{changes}");
    assert!(changes.contains("GME"), "{changes}");
    assert!(changes.contains("valid_until capped"), "{changes}");
    assert!(sim.engine.plan().plan.global_risk_multiplier <= 1.0);
    assert!(sim.engine.plan().plan.watchlist[0].max_risk_pct <= 0.5);

    // Not newer, garbage, and unknown fields are refused with 422.
    for bad in [
        plan_json(1, r#""global_risk_multiplier":0.5"#),
        "{".to_string(),
        plan_json(2, r#""global_risk_multiplier":0.5,"place_order":true"#),
    ] {
        assert_eq!(
            sim.engine.handle(Request::SubmitPlan(bad), now).await.unwrap_err().0,
            422
        );
    }
    let got = sim.engine.handle(Request::Plan, now).await.unwrap();
    assert_eq!(got["plan"]["version"], 1);

    // A restart keeps today's plan.
    drop(sim);
    let paper = paper_at(dir.path());
    let db = Db::open(&dir.path().join("saarthi.db")).unwrap();
    let engine = Engine::new(
        parts(&paper, db, Arc::new(MemoryAlerts::default()), dir.path().join("HALT")),
        now,
    )
    .await
    .unwrap();
    assert_eq!(engine.plan().version(), 1);
    assert!(!engine.plan().is_default);
}

#[tokio::test]
async fn avoid_plan_blocks_the_breakout() {
    use saarthi_control::Request;
    let dir = tempfile::tempdir().unwrap();
    let mut sim = Sim::new(dir.path()).await;
    let p = plan_json(1, r#""market_bias":"avoid","global_risk_multiplier":0.5"#)
        .replace(r#""market_bias":"neutral","#, "");
    sim.engine.handle(Request::SubmitPlan(p), sim.now).await.unwrap();
    sim.run_to_breakout().await;
    assert_eq!(sim.engine.oms().trades().count(), 0);
    let state = sim.engine.handle(Request::State, sim.now).await.unwrap();
    assert_eq!(state["entries_open"], false);
}

#[tokio::test]
async fn watchlist_without_the_symbol_denies_and_records_the_signal() {
    use saarthi_control::Request;
    let dir = tempfile::tempdir().unwrap();
    let mut sim = Sim::new(dir.path()).await;
    // A plan that only wants a short on RELIANCE: the long breakout is denied.
    let p = plan_json(
        1,
        r#""global_risk_multiplier":1.0,"watchlist":[
            {"symbol":"RELIANCE-EQ","strategies":["orb"],"direction":"short_only","max_risk_pct":0.25}]"#,
    );
    sim.engine.handle(Request::SubmitPlan(p), sim.now).await.unwrap();
    sim.run_to_breakout().await;
    assert_eq!(sim.engine.oms().trades().count(), 0);
    let sig = sim.engine.handle(Request::Signals { n: 10 }, sim.now).await.unwrap();
    assert!(sig.to_string().contains("plan_direction"), "{sig}");
}

#[tokio::test]
async fn plan_scales_size_down() {
    use saarthi_control::Request;
    let dir = tempfile::tempdir().unwrap();
    let mut full = Sim::new(dir.path()).await;
    let p = plan_json(1, r#""global_risk_multiplier":1.0"#);
    full.engine.handle(Request::SubmitPlan(p), full.now).await.unwrap();
    full.run_to_breakout().await;
    let q_full = full.engine.oms().trades().next().unwrap().planned_qty;

    let dir2 = tempfile::tempdir().unwrap();
    let mut quarter = Sim::new(dir2.path()).await;
    let p = plan_json(1, r#""global_risk_multiplier":0.25"#);
    quarter
        .engine
        .handle(Request::SubmitPlan(p), quarter.now)
        .await
        .unwrap();
    quarter.run_to_breakout().await;
    let q = quarter.engine.oms().trades().next().unwrap().planned_qty;
    assert!(q < q_full && q > 0, "{q} vs {q_full}");
}
