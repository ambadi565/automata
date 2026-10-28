//! The backtester on synthetic data: invariants, determinism, no look-ahead,
//! no invented edge, and the CLI's outputs.

use std::path::Path;
use std::process::Command;

use backtest::config::{BacktestConfig, RiskLimits, WalkForwardConfig};
use backtest::data::{Dataset, SymbolBars};
use backtest::runner::{self, Env, RunResult, RunSpec};
use backtest::synth::{self, Rng, SynthMode};
use backtest::walkforward;
use chrono::{NaiveDate, NaiveTime};
use saarthi_bars::IndicatorParams;
use saarthi_broker::backtest::ExecutionCosts;
use saarthi_core::{IST, Price, Symbol, Tf};
use saarthi_strategy::{Direction, ParamSet, registry};

fn cfg(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config").join(name)
}

fn env() -> Env {
    let risk = RiskLimits::load(&cfg("risk.toml")).unwrap();
    let costs = ExecutionCosts::load(&cfg("costs.toml")).unwrap();
    Env {
        costs: costs.charges,
        slippage: costs.slippage,
        session: risk.session().unwrap(),
        risk,
        capital: Price(50_000_000), // ₹5 lakh
        indicators: IndicatorParams::default(),
        direction: Direction::Both,
    }
}

fn dataset(symbols: usize, days: usize, seed: u64, mode: SynthMode) -> Dataset {
    let ds = synth::weekdays(NaiveDate::from_ymd_opt(2025, 1, 6).unwrap(), days);
    let mut rng = Rng::new(seed);
    let syms = (0..symbols)
        .map(|k| SymbolBars {
            symbol: Symbol::new(format!("SYN{k:03}-EQ")),
            bars: synth::generate(&mut rng, &ds, mode),
        })
        .collect();
    Dataset::new(Tf::M5, syms)
}

fn single(data: &Dataset, env: &Env, strategy: &str) -> RunResult {
    let params = ParamSet::default();
    runner::run(
        data,
        env,
        &RunSpec {
            strategy,
            params: &params,
            replay_from: data.days[0],
            trade_from: data.days[2],
            to: *data.days.last().unwrap(),
            capital: env.capital,
        },
    )
    .unwrap()
}

#[test]
fn trades_obey_the_session_rules_and_equity_adds_up() {
    let data = dataset(8, 30, 1, SynthMode::Regimes);
    let env = env();
    let no_new = NaiveTime::from_hms_opt(14, 45, 0).unwrap();
    let sq = NaiveTime::from_hms_opt(15, 10, 0).unwrap();
    let mut total = 0;
    for id in registry::IDS {
        let r = single(&data, &env, id);
        total += r.trades.len();
        for t in &r.trades {
            let sig = t.signal_ts.with_timezone(&IST);
            let exit = t.exit_ts.with_timezone(&IST);
            assert!(sig.time() < no_new, "{id}: signal at {sig}");
            assert!(t.entry_ts >= t.signal_ts, "{id}: filled before the signal");
            assert_eq!(sig.date_naive(), exit.date_naive(), "{id}: held overnight");
            assert!(exit.time() <= sq, "{id}: exit at {exit}");
            assert!(sig.date_naive() >= data.days[2], "{id}: traded during warm-up");
            assert_eq!(t.net, t.gross - t.charges);
            assert!(t.charges.0 > 0);
        }
        // Final equity = capital + Σ net P&L (everything is flat at the end).
        let net: i64 = r.trades.iter().map(|t| t.net.0).sum();
        assert_eq!(r.equity.last().unwrap().1, env.capital + Price(net), "{id}");
        assert_eq!(r.daily.len(), data.days.len() - 2);
    }
    assert!(total > 30, "strategies should trade on regime data, got {total}");
}

#[test]
fn deterministic() {
    let data = dataset(5, 15, 2, SynthMode::Regimes);
    let env = env();
    for id in registry::IDS {
        assert_eq!(single(&data, &env, id).trades, single(&data, &env, id).trades, "{id}");
    }
}

/// Changing the future must not change the past: perturb every bar from day
/// 10 on and check that trades closed before then are identical.
#[test]
fn no_look_ahead() {
    let data = dataset(6, 20, 3, SynthMode::Regimes);
    let cut = data.days[10];
    let mut future = data.clone();
    for s in &mut future.symbols {
        for b in &mut s.bars {
            if saarthi_core::MarketClock::ist_date(b.ts) >= cut {
                let k = 3;
                *b = saarthi_core::Bar {
                    open: Price(b.open.0 * k),
                    high: Price(b.high.0 * k),
                    low: Price(b.low.0 * k),
                    close: Price(b.close.0 * k),
                    volume: b.volume * 7,
                    ..*b
                };
            }
        }
    }
    let future = Dataset::new(Tf::M5, future.symbols);
    let env = env();
    for id in registry::IDS {
        let before = |r: RunResult| -> Vec<_> {
            r.trades
                .into_iter()
                .filter(|t| saarthi_core::MarketClock::ist_date(t.exit_ts) < cut)
                .collect()
        };
        let a = before(single(&data, &env, id));
        let b = before(single(&future, &env, id));
        assert!(!a.is_empty(), "{id}: nothing to compare");
        assert_eq!(a, b, "{id}: past trades depend on future bars");
    }
}

/// On a driftless random walk there is nothing to find: after charges and
/// slippage no strategy should look good.
#[test]
fn random_walk_has_no_edge_after_costs() {
    let data = dataset(20, 60, 4, SynthMode::RandomWalk);
    let env = env();
    for id in registry::IDS {
        let r = single(&data, &env, id);
        let m = walkforward::evaluate(&r);
        assert!(m.trades >= 20, "{id}: only {} trades", m.trades);
        assert!(m.profit_factor < 1.3, "{id}: PF {:.2} on noise", m.profit_factor);
        assert!(m.net_pnl < m.gross_pnl, "{id}: charges must cost something");
    }
}

#[test]
fn walk_forward_produces_windows_and_a_gate_verdict() {
    let data = dataset(6, 50, 5, SynthMode::Regimes);
    let env = env();
    let b = BacktestConfig::load(&cfg("backtest.toml")).unwrap();
    let wf = WalkForwardConfig {
        train_days: 20,
        test_days: 10,
        min_train_trades: 5,
    };
    let r = walkforward::walk_forward(&data, &env, "orb", &wf, &b.gate, 2).unwrap();
    assert_eq!(r.windows.len(), 3);
    assert_eq!(r.gate.checks.len(), 4);
    // OOS trades come only from test windows.
    let first_test = r.windows[0].window.test_from;
    assert!(
        r.oos_trades
            .iter()
            .all(|t| saarthi_core::MarketClock::ist_date(t.signal_ts) >= first_test)
    );
    assert_eq!(r.oos.trades, r.oos_trades.len());
    assert_eq!(r.oos.trades, r.windows.iter().map(|w| w.test.trades).sum::<usize>());
    // Stitched equity ends at capital + Σ OOS net.
    let net: i64 = r.oos_trades.iter().map(|t| t.net.0).sum();
    assert_eq!(r.oos_equity.last().unwrap().1, env.capital + Price(net));
}

#[test]
fn cli_synth_then_backtest_writes_reports() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = dir.path().join("out");
    let bin = env!("CARGO_BIN_EXE_backtest");
    let st = Command::new(bin)
        .args(["synth", "--symbols", "4", "--days", "45", "--data-dir"])
        .arg(&data)
        .status()
        .unwrap();
    assert!(st.success());
    let o = Command::new(bin)
        .args(["--strategy", "orb,momentum", "--walk-forward", "20:10"])
        .arg("--config")
        .arg(cfg("backtest.toml"))
        .arg("--risk")
        .arg(cfg("risk.toml"))
        .arg("--costs")
        .arg(cfg("costs.toml"))
        .arg("--data-dir")
        .arg(&data)
        .arg("--universe")
        .arg(data.join("synthetic_universe.toml"))
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&o.stderr));
    assert!(stdout.contains("SYNTHETIC DATA"), "synthetic results must say so");
    for s in ["orb", "momentum"] {
        for f in [
            "trades.csv",
            "equity.csv",
            "equity.svg",
            "windows.csv",
            "summary.json",
            "report.md",
        ] {
            assert!(out.join(s).join(f).exists(), "{s}/{f}");
        }
        let j: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join(s).join("summary.json")).unwrap()).unwrap();
        assert!(j["gate"]["pass"].is_boolean());
        assert!(j["data_note"].as_str().unwrap().contains("SYNTHETIC"));
    }
    assert!(out.join("overview.md").exists());

    // --fail-on-gate turns a failed gate into exit status 2 (4 symbols over
    // 25 test days can't reach 200 trades).
    let o = Command::new(bin)
        .args(["--strategy", "orb", "--walk-forward", "20:10", "--fail-on-gate"])
        .arg("--config")
        .arg(cfg("backtest.toml"))
        .arg("--risk")
        .arg(cfg("risk.toml"))
        .arg("--costs")
        .arg(cfg("costs.toml"))
        .arg("--data-dir")
        .arg(&data)
        .arg("--universe")
        .arg(data.join("synthetic_universe.toml"))
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}
