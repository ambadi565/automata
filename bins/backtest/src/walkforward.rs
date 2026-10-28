//! Walk-forward optimisation and the M2 go-live gate.
//!
//! The data's trading days are cut into rolling windows: optimise on `train`
//! days (every point of the strategy's parameter grid), trade the next
//! `test` days with the winner, slide forward by `test` days. Only the test
//! segments, stitched together, count as out-of-sample (OOS). The gate is
//! judged on OOS results alone.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use chrono::NaiveDate;
use saarthi_core::{Price, Ts};
use saarthi_strategy::{ParamGrid, ParamSet, registry};
use serde::Serialize;

use crate::config::{GateConfig, WalkForwardConfig};
use crate::data::Dataset;
use crate::metrics::{self, Metrics};
use crate::runner::{self, Env, RunResult, RunSpec, Trade};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Window {
    pub index: usize,
    pub train_from: NaiveDate,
    pub train_to: NaiveDate,
    pub test_from: NaiveDate,
    pub test_to: NaiveDate,
}

/// Rolling windows over `days`. The last test segment may be shorter than
/// `test`; a window needs at least one test day.
pub fn windows(days: &[NaiveDate], train: usize, test: usize) -> Vec<Window> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + train < days.len() {
        let test_end = (i + train + test).min(days.len());
        out.push(Window {
            index: out.len(),
            train_from: days[i],
            train_to: days[i + train - 1],
            test_from: days[i + train],
            test_to: days[test_end - 1],
        });
        i += test;
    }
    out
}

/// In-sample ranking score: return over drawdown (Calmar-like), so a
/// parameter set can't win by taking more risk. Sets with too few trades are
/// ineligible.
pub fn score(m: &Metrics, min_trades: usize) -> f64 {
    if m.trades < min_trades || m.trades == 0 {
        return f64::NEG_INFINITY;
    }
    m.return_pct / m.max_drawdown_pct.max(0.5)
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowResult {
    pub window: Window,
    pub params: ParamSet,
    pub train: Metrics,
    pub test: Metrics,
    /// Grid neighbours of the chosen set, and how many were profitable
    /// in-sample (net P&L > 0 and PF ≥ 1).
    pub neighbours: usize,
    pub neighbours_profitable: usize,
    /// Every eligible grid point had too few trades: the window used
    /// default parameters.
    pub fallback_defaults: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateCheck {
    pub name: &'static str,
    pub value: f64,
    pub threshold: f64,
    pub pass: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Gate {
    pub pass: bool,
    pub checks: Vec<GateCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardResult {
    pub strategy: String,
    pub windows: Vec<WindowResult>,
    pub oos: Metrics,
    #[serde(skip)]
    pub oos_trades: Vec<Trade>,
    /// Stitched OOS equity: each test segment trades the equity the
    /// previous one ended with.
    #[serde(skip)]
    pub oos_equity: Vec<(Ts, Price)>,
    #[serde(skip)]
    pub oos_daily: Vec<(NaiveDate, Price)>,
    pub neighbour_profitable_share: f64,
    /// Distinct parameter sets chosen across windows.
    pub distinct_param_sets: usize,
    pub oos_profitable_windows: usize,
    pub gate: Gate,
}

fn warmup_start(days: &[NaiveDate], from: NaiveDate, warmup: usize) -> NaiveDate {
    let i = days.partition_point(|d| *d < from);
    days[i.saturating_sub(warmup)]
}

/// Runs `f` over `items` on all cores, keeping order.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(items.len().max(1));
    let chunk = items.len().div_ceil(n.max(1)).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|c| {
                let f = &f;
                s.spawn(move || c.iter().map(f).collect::<Vec<R>>())
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("backtest worker panicked"))
            .collect()
    })
}

pub fn evaluate(r: &RunResult) -> Metrics {
    metrics::compute(&r.trades, r.capital, &r.equity, &r.daily)
}

fn profitable(m: &Metrics) -> bool {
    m.net_pnl > 0.0 && m.profit_factor >= 1.0
}

pub fn walk_forward(
    data: &Dataset,
    env: &Env,
    strategy: &str,
    wf: &WalkForwardConfig,
    gate_cfg: &GateConfig,
    warmup_days: usize,
) -> Result<WalkForwardResult> {
    let grid: ParamGrid = registry::grid(strategy)?;
    let combos = grid.combos();
    let defaults = registry::defaults(strategy)?;
    let wins = windows(&data.days, wf.train_days, wf.test_days);
    if wins.is_empty() {
        bail!(
            "{} trading days of data; walk-forward {}:{} needs more than {}",
            data.days.len(),
            wf.train_days,
            wf.test_days,
            wf.train_days
        );
    }
    let mut results = Vec::new();
    let mut oos_trades = Vec::new();
    let mut oos_equity: Vec<(Ts, Price)> = Vec::new();
    let mut oos_daily = Vec::new();
    // Out-of-sample equity compounds: each test segment starts with what the
    // previous one ended with.
    let mut equity = env.capital;
    for w in &wins {
        let replay_from = warmup_start(&data.days, w.train_from, warmup_days);
        let runs: Vec<Result<Metrics>> = par_map(&combos, |p| {
            let spec = RunSpec {
                strategy,
                params: p,
                replay_from,
                trade_from: w.train_from,
                to: w.train_to,
                capital: env.capital,
            };
            Ok(evaluate(&runner::run(data, env, &spec)?))
        });
        let mut by_key: BTreeMap<String, Metrics> = BTreeMap::new();
        let mut best: Option<(f64, usize)> = None;
        if equity.0 <= 0 {
            bail!("{strategy}: out-of-sample equity is gone by window {}", w.index);
        }
        for (k, r) in runs.into_iter().enumerate() {
            let m = r?;
            let sc = score(&m, wf.min_train_trades);
            if sc.is_finite() && best.is_none_or(|(b, _)| sc > b) {
                best = Some((sc, k));
            }
            by_key.insert(combos[k].to_string(), m);
        }
        let (params, fallback) = match best {
            Some((_, k)) => (combos[k].clone(), false),
            None => (defaults.clone(), true),
        };
        let train = match by_key.get(&params.to_string()) {
            Some(m) => m.clone(),
            None => evaluate(&runner::run(
                data,
                env,
                &RunSpec {
                    strategy,
                    params: &params,
                    replay_from,
                    trade_from: w.train_from,
                    to: w.train_to,
                    capital: env.capital,
                },
            )?),
        };
        let nbrs = grid.neighbours(&params);
        let nbr_ok = nbrs
            .iter()
            .filter(|p| by_key.get(&p.to_string()).is_some_and(profitable))
            .count();

        let test_run = runner::run(
            data,
            env,
            &RunSpec {
                strategy,
                params: &params,
                replay_from: warmup_start(&data.days, w.test_from, warmup_days),
                trade_from: w.test_from,
                to: w.test_to,
                capital: equity,
            },
        )?;
        let test = evaluate(&test_run);
        oos_equity.extend_from_slice(&test_run.equity);
        oos_daily.extend_from_slice(&test_run.daily);
        if let Some(&(_, last)) = test_run.daily.last() {
            equity = last;
        }
        oos_trades.extend(test_run.trades);
        tracing::info!(
            strategy,
            window = w.index,
            test_from = %w.test_from,
            params = %params,
            train_pf = format!("{:.2}", train.profit_factor),
            test_pf = format!("{:.2}", test.profit_factor),
            test_trades = test.trades,
            "window done"
        );
        results.push(WindowResult {
            window: *w,
            params,
            train,
            test,
            neighbours: nbrs.len(),
            neighbours_profitable: nbr_ok,
            fallback_defaults: fallback,
        });
    }
    let capital = env.capital;
    let oos = metrics::compute(&oos_trades, capital, &oos_equity, &oos_daily);
    let total_n: usize = results.iter().map(|r| r.neighbours).sum();
    let ok_n: usize = results.iter().map(|r| r.neighbours_profitable).sum();
    let share = if total_n == 0 {
        0.0
    } else {
        ok_n as f64 / total_n as f64
    };
    let mut distinct: Vec<String> = results.iter().map(|r| r.params.to_string()).collect();
    distinct.sort();
    distinct.dedup();
    let gate = gate(&oos, share, gate_cfg);
    Ok(WalkForwardResult {
        strategy: strategy.to_string(),
        oos_profitable_windows: results.iter().filter(|r| r.test.net_pnl > 0.0).count(),
        windows: results,
        oos,
        oos_trades,
        oos_equity,
        oos_daily,
        neighbour_profitable_share: share,
        distinct_param_sets: distinct.len(),
        gate,
    })
}

/// PLAN M2: OOS PF > 1.3, max DD < 8 %, ≥ 200 trades, stable parameters.
pub fn gate(oos: &Metrics, neighbour_share: f64, g: &GateConfig) -> Gate {
    let checks = vec![
        GateCheck {
            name: "oos_profit_factor",
            value: oos.profit_factor,
            threshold: g.min_profit_factor,
            pass: oos.profit_factor > g.min_profit_factor,
        },
        GateCheck {
            name: "oos_max_drawdown_pct",
            value: oos.max_drawdown_pct,
            threshold: g.max_drawdown_pct,
            pass: oos.max_drawdown_pct < g.max_drawdown_pct,
        },
        GateCheck {
            name: "oos_trades",
            value: oos.trades as f64,
            threshold: g.min_trades as f64,
            pass: oos.trades >= g.min_trades,
        },
        GateCheck {
            name: "neighbour_profitable_share",
            value: neighbour_share,
            threshold: g.min_neighbour_profitable_share,
            pass: neighbour_share >= g.min_neighbour_profitable_share,
        },
    ];
    Gate {
        pass: checks.iter().all(|c| c.pass),
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(n: u32) -> Vec<NaiveDate> {
        let d0 = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        (0..n).map(|i| d0 + chrono::Days::new(u64::from(i))).collect()
    }

    #[test]
    fn windows_roll_by_test_length_without_overlap_in_test() {
        let d = days(100);
        let w = windows(&d, 60, 20);
        assert_eq!(w.len(), 2);
        assert_eq!((w[0].train_from, w[0].test_from, w[0].test_to), (d[0], d[60], d[79]));
        assert_eq!((w[1].train_from, w[1].test_from, w[1].test_to), (d[20], d[80], d[99]));
        // A short tail still gets tested.
        let w = windows(&days(90), 60, 20);
        assert_eq!(w.len(), 2);
        assert_eq!(w[1].test_to, days(90)[89]);
        assert!(windows(&days(60), 60, 20).is_empty());
    }

    #[test]
    fn gate_requires_every_check() {
        let g = GateConfig {
            min_profit_factor: 1.3,
            max_drawdown_pct: 8.0,
            min_trades: 200,
            min_neighbour_profitable_share: 0.6,
        };
        let good = Metrics {
            trades: 250,
            profit_factor: 1.5,
            max_drawdown_pct: 5.0,
            ..Metrics::default()
        };
        assert!(gate(&good, 0.8, &g).pass);
        assert!(!gate(&good, 0.5, &g).pass, "unstable parameters");
        let few = Metrics {
            trades: 199,
            ..good.clone()
        };
        assert!(!gate(&few, 0.8, &g).pass);
        let edge_pf = Metrics {
            profit_factor: 1.3,
            ..good.clone()
        };
        assert!(!gate(&edge_pf, 0.8, &g).pass, "PF must be strictly above 1.3");
        let deep = Metrics {
            max_drawdown_pct: 8.0,
            ..good
        };
        assert!(!gate(&deep, 0.8, &g).pass, "DD must be strictly below 8%");
    }
}
