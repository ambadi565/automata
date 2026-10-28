use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use backtest::config::{BacktestConfig, RiskLimits, WalkForwardConfig};
use backtest::data::Dataset;
use backtest::runner::{Env, RunSpec};
use backtest::synth::{self, SynthMode};
use backtest::{SYNTHETIC_MARKER, SYNTHETIC_NOTE, load_universe, report, runner, walkforward};
use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand};
use saarthi_bars::IndicatorParams;
use saarthi_broker::backtest::ExecutionCosts;
use saarthi_core::{Price, Symbol};
use saarthi_store::DataLayout;
use saarthi_strategy::{Direction, ParamSet, registry};

/// Replays historical candles through the strategies with Indian intraday
/// charges and slippage, optimises walk-forward and checks the go-live gate.
/// Never places real orders.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate synthetic 5-minute candles (for testing the pipeline only).
    Synth(SynthArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Strategy id, a comma-separated list, or "all".
    #[arg(long, default_value = "all")]
    strategy: String,
    /// First IST date (default: earliest data).
    #[arg(long)]
    from: Option<NaiveDate>,
    /// Last IST date, inclusive (default: latest data).
    #[arg(long)]
    to: Option<NaiveDate>,
    /// TRAIN:TEST trading days (e.g. 60:20), or "off" for one in-sample run
    /// with --params / defaults. Default from backtest.toml.
    #[arg(long)]
    walk_forward: Option<String>,
    /// Parameters for a single run, e.g. "atr_stop=1.5,rr=2".
    #[arg(long)]
    params: Option<String>,
    #[arg(long, value_enum, default_value = "both")]
    direction: DirArg,
    #[arg(long, default_value = "config/backtest.toml")]
    config: PathBuf,
    #[arg(long, default_value = "config/risk.toml")]
    risk: PathBuf,
    #[arg(long, default_value = "config/costs.toml")]
    costs: PathBuf,
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    #[arg(long, default_value = "config/universe.toml")]
    universe: PathBuf,
    /// Comma-separated symbols; overrides --universe.
    #[arg(long)]
    symbols: Option<String>,
    /// Capital in rupees (default: risk.toml capital_base_inr, else backtest.toml).
    #[arg(long)]
    capital: Option<f64>,
    #[arg(long, default_value = "data/reports/backtest")]
    out: PathBuf,
    /// Exit with status 2 unless every strategy passes the gate.
    #[arg(long)]
    fail_on_gate: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum DirArg {
    Both,
    LongOnly,
    ShortOnly,
}

#[derive(Args)]
struct SynthArgs {
    #[arg(long, default_value = "data")]
    data_dir: PathBuf,
    #[arg(long, default_value_t = 30)]
    symbols: usize,
    /// Trading days (weekdays) to generate.
    #[arg(long, default_value_t = 250)]
    days: usize,
    #[arg(long, default_value = "2025-01-06")]
    start: NaiveDate,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long, value_enum, default_value = "regimes")]
    mode: SynthMode,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "backtest=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let res = match cli.cmd {
        Some(Cmd::Synth(a)) => synth_cmd(a).map(|()| true),
        None => run_cmd(cli.run),
    };
    match res {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn synth_cmd(a: SynthArgs) -> Result<()> {
    let days = synth::weekdays(a.start, a.days);
    let syms = synth::write(&a.data_dir, a.symbols, &days, a.seed, a.mode)?;
    let dir = DataLayout::new(&a.data_dir).candles_dir(saarthi_core::Tf::M5);
    std::fs::write(
        dir.join(SYNTHETIC_MARKER),
        format!("{SYNTHETIC_NOTE}\nmode={:?} seed={}\n", a.mode, a.seed),
    )?;
    let list: Vec<String> = syms.iter().map(|s| format!("\"{s}\"")).collect();
    let uni = a.data_dir.join("synthetic_universe.toml");
    std::fs::write(
        &uni,
        format!(
            "# Synthetic symbols written by `backtest synth`. Not real instruments.\nname = \"SYNTHETIC\"\nsymbols = [{}]\n",
            list.join(", ")
        ),
    )?;
    println!(
        "wrote {} synthetic symbols x {} days ({} to {}) under {}\nuniverse: {}\n\n{SYNTHETIC_NOTE}",
        syms.len(),
        days.len(),
        days[0],
        days[days.len() - 1],
        dir.display(),
        uni.display()
    );
    Ok(())
}

fn parse_params(s: &str) -> Result<ParamSet> {
    let mut p = ParamSet::default();
    for kv in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("bad param {kv:?}; want name=value"))?;
        p.set(
            k.trim(),
            v.trim().parse().with_context(|| format!("bad value in {kv:?}"))?,
        );
    }
    Ok(p)
}

fn run_cmd(a: RunArgs) -> Result<bool> {
    let cfg = BacktestConfig::load(&a.config)?;
    let risk = RiskLimits::load(&a.risk)?;
    let costs = ExecutionCosts::load(&a.costs)?;
    let strategies: Vec<String> = if a.strategy == "all" {
        registry::IDS.iter().map(|s| s.to_string()).collect()
    } else {
        a.strategy.split(',').map(|s| s.trim().to_string()).collect()
    };
    for s in &strategies {
        registry::specs(s)?;
    }
    let symbols: Vec<Symbol> = match &a.symbols {
        Some(s) => s.split(',').map(|x| Symbol::new(x.trim())).collect(),
        None => load_universe(&a.universe)?,
    };
    let from = a.from.unwrap_or(NaiveDate::MIN);
    let to = a.to.unwrap_or(NaiveDate::MAX);
    let data = Dataset::load(&a.data_dir, cfg.tf, &symbols, from, to)?;
    if data.events.is_empty() {
        bail!(
            "no {} candles for these symbols in {} — run `fetch candles` (or `backtest synth` to test the pipeline)",
            cfg.tf,
            DataLayout::new(&a.data_dir).candles_dir(cfg.tf).display()
        );
    }
    let synthetic = DataLayout::new(&a.data_dir)
        .candles_dir(cfg.tf)
        .join(SYNTHETIC_MARKER)
        .exists()
        || data.symbols.iter().any(|s| s.symbol.as_str().starts_with("SYN"));
    let note = if synthetic { SYNTHETIC_NOTE } else { "" };
    if synthetic {
        eprintln!("warning: {SYNTHETIC_NOTE}\n");
    }
    let capital_inr = a.capital.unwrap_or(if risk.capital_base_inr > 0.0 {
        risk.capital_base_inr
    } else {
        cfg.capital_inr
    });
    let env = Env {
        costs: costs.charges,
        slippage: costs.slippage,
        session: risk.session()?,
        risk,
        capital: Price((capital_inr * 100.0).round() as i64),
        indicators: IndicatorParams::default(),
        direction: match a.direction {
            DirArg::Both => Direction::Both,
            DirArg::LongOnly => Direction::LongOnly,
            DirArg::ShortOnly => Direction::ShortOnly,
        },
    };
    let header = format!(
        "{} symbols, {} trading days ({} to {}), {} bars of {}; capital ₹{capital_inr:.0}; costs as of {}.{}",
        data.symbols.len(),
        data.days.len(),
        data.days[0],
        data.days[data.days.len() - 1],
        data.bar_count(),
        data.tf,
        env.costs.as_of,
        if synthetic {
            format!("\n\n> {SYNTHETIC_NOTE}")
        } else {
            String::new()
        }
    );
    eprintln!("{header}\n");

    let wf_arg = a
        .walk_forward
        .unwrap_or_else(|| format!("{}:{}", cfg.walk_forward.train_days, cfg.walk_forward.test_days));
    if wf_arg == "off" {
        for s in &strategies {
            let params = match &a.params {
                Some(p) => parse_params(p)?.with_defaults(&registry::defaults(s)?),
                None => registry::defaults(s)?,
            };
            let spec = RunSpec {
                strategy: s,
                params: &params,
                replay_from: data.days[0],
                trade_from: data.days[cfg.warmup_days.min(data.days.len() - 1)],
                to: data.days[data.days.len() - 1],
                capital: env.capital,
            };
            let r = runner::run(&data, &env, &spec)?;
            let m = walkforward::evaluate(&r);
            report::write_single(&a.out.join(s), s, &params.to_string(), &m, &r)?;
            println!(
                "{s:<12} single run ({params}): {} trades, PF {:.2}, net ₹{:.0}, max DD {:.2} %, Sharpe {:.2}. In-sample: no gate verdict.",
                m.trades, m.profit_factor, m.net_pnl, m.max_drawdown_pct, m.sharpe
            );
        }
        println!("\nreports: {}", a.out.display());
        return Ok(true);
    }
    if a.params.is_some() {
        bail!("--params only applies with --walk-forward off; walk-forward chooses parameters itself");
    }
    let (train, test) = wf_arg
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .filter(|&(a, b): &(usize, usize)| a > 0 && b > 0)
        .with_context(|| format!("--walk-forward wants TRAIN:TEST days, e.g. 60:20, got {wf_arg:?}"))?;
    let wf = WalkForwardConfig {
        train_days: train,
        test_days: test,
        ..cfg.walk_forward
    };
    let mut results = Vec::new();
    for s in &strategies {
        let r = walkforward::walk_forward(&data, &env, s, &wf, &cfg.gate, cfg.warmup_days)?;
        report::write_walk_forward(&a.out.join(s), &r, env.capital, note)?;
        results.push(r);
    }
    let md = report::write_overview(&a.out, &results, &header)?;
    println!("{md}\nreports: {}", a.out.display());
    Ok(!a.fail_on_gate || results.iter().all(|r| r.gate.pass))
}
