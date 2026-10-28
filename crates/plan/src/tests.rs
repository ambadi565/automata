use std::collections::BTreeSet;
use std::path::Path;

use chrono::{NaiveTime, TimeDelta};
use saarthi_core::{HolidayCalendar, MarketClock, Side, Symbol};
use saarthi_risk::RiskLimits;

use super::*;

fn cfg(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config").join(name)
}

struct Env {
    limits: RiskLimits,
    clock: MarketClock,
    symbols: BTreeSet<Symbol>,
    strategies: BTreeSet<String>,
    now: Ts,
}

fn env() -> Env {
    let limits = RiskLimits::load(&cfg("risk.toml")).unwrap();
    let clock = MarketClock::new(
        limits.session().unwrap(),
        HolidayCalendar::load(&cfg("holidays.toml")).unwrap(),
    );
    Env {
        limits,
        clock,
        symbols: ["RELIANCE-EQ", "TCS-EQ", "INFY-EQ"].map(Symbol::new).into(),
        strategies: ["orb", "vwap_revert", "momentum"].map(String::from).into(),
        now: MarketClock::at(
            NaiveDate::from_ymd_opt(2026, 10, 28).unwrap(),
            NaiveTime::from_hms_opt(8, 50, 0).unwrap(),
        ),
    }
}

impl Env {
    fn ctx(&self, current: u64) -> Context<'_> {
        Context {
            now: self.now,
            limits: &self.limits,
            clock: &self.clock,
            symbols: &self.symbols,
            strategies: &self.strategies,
            current_version: current,
        }
    }
}

const GOOD: &str = r#"{
  "version": 7,
  "valid_until": "2026-10-28T15:10:00+05:30",
  "market_bias": "bullish",
  "regime": "trend",
  "global_risk_multiplier": 0.8,
  "watchlist": [
    {"symbol": "RELIANCE-EQ", "strategies": ["orb", "momentum"], "direction": "long_only",
     "max_risk_pct": 0.25, "key_levels": {"support": [2810.5], "resistance": [2865.0]},
     "notes": "Results beat; gap up expected"}
  ],
  "paused_strategies": ["vwap_revert"],
  "rationale": "Trend day setup"
}"#;

#[test]
fn good_plan_passes_unchanged() {
    let e = env();
    let p = validate(parse(GOOD).unwrap(), &e.ctx(6), false).unwrap();
    assert!(p.changes.is_empty(), "{:?}", p.changes);
    let rel = Symbol::new("RELIANCE-EQ");
    // 0.8 × (0.25 / 0.5) = 0.4 of the guard's budget.
    match p.verdict(&rel, "orb", Side::Buy, &e.limits) {
        Verdict::Allow { risk_scale } => assert!((risk_scale - 0.4).abs() < 1e-9),
        v => panic!("{v:?}"),
    }
    assert_eq!(
        p.verdict(&rel, "orb", Side::Sell, &e.limits),
        Verdict::Deny("plan_direction")
    );
    assert_eq!(
        p.verdict(&rel, "vwap_revert", Side::Buy, &e.limits),
        Verdict::Deny("plan_paused")
    );
    assert_eq!(
        p.verdict(&Symbol::new("TCS-EQ"), "orb", Side::Buy, &e.limits),
        Verdict::Deny("plan_not_on_watchlist")
    );
}

#[test]
fn garbage_is_rejected() {
    for bad in [
        "",
        "not json",
        "[]",
        r#"{"version": 7}"#,
        &GOOD.replace("\"regime\"", "\"regime_typo\""),
        &GOOD.replace("\"bullish\"", "\"yolo\""),
        &GOOD.replace("\"notes\"", "\"order\""),
        &GOOD.replace("0.8", "\"lots\""),
    ] {
        assert!(matches!(parse(bad), Err(PlanError::Parse(_))), "{bad}");
    }
}

#[test]
fn stale_or_expired_plans_are_rejected() {
    let e = env();
    let p = parse(GOOD).unwrap();
    assert!(validate(p.clone(), &e.ctx(7), false).is_err(), "same version");
    let mut old = p.clone();
    old.valid_until = Some("2026-10-28T08:00:00+05:30".parse().unwrap());
    assert!(validate(old, &e.ctx(0), false).is_err(), "expired");
    let mut none = p;
    none.valid_until = None;
    assert!(validate(none, &e.ctx(0), false).is_err(), "expiry required");
}

#[test]
fn oversized_requests_are_clamped_not_obeyed() {
    let e = env();
    let mut p = parse(GOOD).unwrap();
    p.global_risk_multiplier = 3.0;
    p.watchlist[0].max_risk_pct = 4.0;
    p.valid_until = Some("2026-10-30T15:30:00+05:30".parse().unwrap());
    p.rationale = "x".repeat(5000);
    let v = validate(p, &e.ctx(0), false).unwrap();
    assert!((v.plan.global_risk_multiplier - 1.0).abs() < 1e-12);
    assert!((v.plan.watchlist[0].max_risk_pct - 0.5).abs() < 1e-12);
    assert_eq!(
        v.valid_until,
        MarketClock::at(MarketClock::ist_date(e.now), e.clock.times.square_off)
    );
    assert_eq!(v.plan.rationale.len(), MAX_TEXT);
    assert_eq!(v.changes.len(), 4, "{:?}", v.changes);
    // Clamped to the limit means at most the guard's own budget.
    match v.verdict(&Symbol::new("RELIANCE-EQ"), "orb", Side::Buy, &e.limits) {
        Verdict::Allow { risk_scale } => assert!(risk_scale <= 1.0),
        v => panic!("{v:?}"),
    }
}

#[test]
fn negative_and_nan_values() {
    let e = env();
    let mut p = parse(GOOD).unwrap();
    p.global_risk_multiplier = -1.0;
    let v = validate(p.clone(), &e.ctx(0), false).unwrap();
    assert!(v.sits_out());
    p.global_risk_multiplier = f64::NAN;
    assert!(validate(p.clone(), &e.ctx(0), false).is_err());
    p.global_risk_multiplier = 0.5;
    p.watchlist[0].max_risk_pct = f64::INFINITY;
    assert!(validate(p, &e.ctx(0), false).is_err());
}

#[test]
fn unknown_symbols_and_strategies_are_dropped() {
    let e = env();
    let mut p = parse(GOOD).unwrap();
    let mut extra = p.watchlist[0].clone();
    extra.symbol = Symbol::new("PUMPNDUMP-EQ");
    p.watchlist.push(extra);
    let mut dup = p.watchlist[0].clone();
    dup.max_risk_pct = 0.5;
    p.watchlist.push(dup);
    let mut odd = p.watchlist[0].clone();
    odd.symbol = Symbol::new("TCS-EQ");
    odd.strategies = vec!["martingale".into()];
    p.watchlist.push(odd);
    p.paused_strategies.push("hft".into());
    let v = validate(p, &e.ctx(0), false).unwrap();
    assert_eq!(v.plan.watchlist.len(), 1);
    assert_eq!(v.plan.watchlist[0].symbol.as_str(), "RELIANCE-EQ");
    assert!(
        (v.plan.watchlist[0].max_risk_pct - 0.25).abs() < 1e-12,
        "first entry kept"
    );
    assert_eq!(v.plan.paused_strategies, vec!["vwap_revert".to_string()]);
    assert!(v.changes.len() >= 4);
}

#[test]
fn watchlist_is_capped() {
    let mut e = env();
    let mut p = parse(GOOD).unwrap();
    p.watchlist.clear();
    for i in 0..30 {
        let s = Symbol::new(format!("S{i}-EQ"));
        e.symbols.insert(s.clone());
        p.watchlist.push(WatchItem {
            symbol: s,
            strategies: vec!["orb".into()],
            direction: Direction::Both,
            max_risk_pct: 0.1,
            key_levels: KeyLevels::default(),
            notes: String::new(),
        });
    }
    let v = validate(p, &e.ctx(0), false).unwrap();
    assert_eq!(v.plan.watchlist.len(), MAX_WATCHLIST);
}

#[test]
fn avoid_sits_out() {
    let e = env();
    let p = parse(&GOOD.replace("\"bullish\"", "\"avoid\"")).unwrap();
    let v = validate(p, &e.ctx(0), false).unwrap();
    assert!(v.sits_out());
    assert_eq!(
        v.verdict(&Symbol::new("RELIANCE-EQ"), "orb", Side::Buy, &e.limits),
        Verdict::Deny("plan_avoid")
    );
}

#[test]
fn shipped_default_plan_is_conservative() {
    let e = env();
    let d = load(&cfg("plan.default.json")).unwrap();
    let v = validate(d, &e.ctx(0), true).unwrap();
    assert!(v.is_default && v.changes.is_empty());
    assert!(v.plan.global_risk_multiplier <= 0.5);
    match v.verdict(&Symbol::new("TCS-EQ"), "momentum", Side::Sell, &e.limits) {
        Verdict::Allow { risk_scale } => assert!(risk_scale <= 0.5),
        v => panic!("{v:?}"),
    }
    assert!(!v.expired(e.now));
    assert!(v.expired(e.now + TimeDelta::hours(7)));
}

#[test]
fn schema_and_types_agree_on_the_example() {
    // The JSON Schema's enums match the Rust enums and strategy ids.
    let schema: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cfg("plan.schema.json")).unwrap()).unwrap();
    let props = &schema["properties"];
    let enum_of = |v: &serde_json::Value| -> Vec<String> {
        v["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect()
    };
    for b in enum_of(&props["market_bias"]) {
        serde_json::from_value::<Bias>(serde_json::Value::String(b)).unwrap();
    }
    for r in enum_of(&props["regime"]) {
        serde_json::from_value::<Regime>(serde_json::Value::String(r)).unwrap();
    }
    let strategies = enum_of(&props["paused_strategies"]["items"]);
    let ids: Vec<String> = saarthi_strategy::registry::IDS.iter().map(|s| s.to_string()).collect();
    assert_eq!(strategies, ids);
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    let example: serde_json::Value = serde_json::from_str(GOOD).unwrap();
    for k in required {
        assert!(example.get(k).is_some(), "example lacks {k}");
    }
}
