//! `fetch`: instrument master and historical candles → Parquet.
//!
//! ```text
//! fetch                       # bootstrap: instruments + 1 year of 5m candles for the universe
//! fetch instruments
//! fetch candles --tf 5m --days 365
//! fetch candles --tf 1d --days 730 --symbols RELIANCE-EQ,TCS-EQ
//! fetch login-check           # verify credentials (read-only)
//! fetch universe --csv ind_nifty100list.csv --name "NIFTY 100"
//! ```
//!
//! Each user/account keeps its own config directory and secrets file, chosen
//! with `--config` / `--secrets` (or `SAARTHI_CONFIG` / `SAARTHI_SECRETS`).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use fetch::config::EngineConfig;
use fetch::universe::Universe;
use fetch::{CandleJob, fetch_candles, fetch_instruments, load_or_fetch_instruments, missing_symbols};
use saarthi_broker::Broker;
use saarthi_broker::smartapi::{Credentials, SmartApi};
use saarthi_core::{MarketClock, Symbol, Tf};
use saarthi_store::DataLayout;

#[derive(Parser)]
#[command(name = "fetch", version, about = "Saarthi market-data fetcher (Angel One SmartAPI)")]
struct Cli {
    /// Engine config (paths, SmartAPI tunables).
    #[arg(long, env = "SAARTHI_CONFIG", default_value = "config/engine.toml", global = true)]
    config: PathBuf,
    /// Account secrets (dotenv format, chmod 600). Env vars override it.
    #[arg(long, env = "SAARTHI_SECRETS", default_value = "config/secrets.env", global = true)]
    secrets: PathBuf,
    /// Overrides `paths.data_dir`.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download the instrument master to data/instruments.parquet.
    Instruments,
    /// Download historical candles (resumes where the last run stopped).
    Candles(CandleArgs),
    /// Log in and print the account name and available cash. Places no orders.
    LoginCheck,
    /// Regenerate universe.toml from NSE's official constituent CSV.
    Universe {
        #[arg(long)]
        csv: PathBuf,
        #[arg(long, default_value = "NIFTY 100")]
        name: String,
        /// Defaults to `paths.universe`.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(clap::Args, Clone)]
struct CandleArgs {
    /// Universe file; defaults to `paths.universe`.
    #[arg(long)]
    universe: Option<PathBuf>,
    #[arg(long, default_value = "5m")]
    tf: Tf,
    /// How many calendar days back from `--to`.
    #[arg(long, default_value_t = 365, conflicts_with = "from")]
    days: i64,
    /// Start date (YYYY-MM-DD, IST).
    #[arg(long)]
    from: Option<NaiveDate>,
    /// End date (YYYY-MM-DD, IST); defaults to today.
    #[arg(long)]
    to: Option<NaiveDate>,
    /// Comma-separated subset of the universe, e.g. RELIANCE-EQ,TCS-EQ.
    #[arg(long, value_delimiter = ',')]
    symbols: Vec<String>,
    /// Re-download the instrument master first.
    #[arg(long)]
    refresh_instruments: bool,
}

impl Default for CandleArgs {
    fn default() -> Self {
        CandleArgs {
            universe: None,
            tf: Tf::M5,
            days: 365,
            from: None,
            to: None,
            symbols: vec![],
            refresh_instruments: true,
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    if std::env::var("SAARTHI_LOG_JSON").is_ok_and(|v| v == "1") {
        b.json().init();
    } else {
        b.init();
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let cfg = EngineConfig::load(&cli.config)?;
    let layout = DataLayout::new(cli.data_dir.clone().unwrap_or_else(|| cfg.paths.data_dir.clone()));
    let api = || -> Result<SmartApi> {
        let secrets = cli.secrets.exists().then_some(cli.secrets.as_path());
        let creds = Credentials::load(secrets)
            .with_context(|| format!("loading credentials (secrets file: {})", cli.secrets.display()))?;
        Ok(SmartApi::new(cfg.smartapi.clone(), creds)?)
    };

    match cli.cmd {
        Some(Cmd::Instruments) => {
            fetch_instruments(&cfg.smartapi, &layout).await?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::LoginCheck) => {
            let api = api()?;
            api.login().await?;
            let profile = api.profile().await?;
            let funds = api.funds().await?;
            println!("Logged in as {} ({})", profile.name, profile.client_code);
            println!("Available cash: ₹{}", funds.available_cash);
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Universe { csv, name, out }) => {
            let text = std::fs::read_to_string(&csv).with_context(|| format!("reading {}", csv.display()))?;
            let today = MarketClock::ist_date(Utc::now()).to_string();
            let u = Universe::from_nse_csv(&name, &text, &today)?;
            let out = out.unwrap_or(cfg.paths.universe.clone());
            std::fs::write(&out, u.to_toml()?).with_context(|| format!("writing {}", out.display()))?;
            println!("Wrote {} symbols to {}", u.symbols.len(), out.display());
            Ok(ExitCode::SUCCESS)
        }
        Some(Cmd::Candles(args)) => candles(&cfg, &layout, &api()?, args).await,
        None => {
            tracing::info!("bootstrap: instrument master + 1 year of 5-minute candles");
            candles(&cfg, &layout, &api()?, CandleArgs::default()).await
        }
    }
}

async fn candles(cfg: &EngineConfig, layout: &DataLayout, api: &SmartApi, args: CandleArgs) -> Result<ExitCode> {
    let universe = Universe::load(args.universe.as_ref().unwrap_or(&cfg.paths.universe))?;
    let instruments = load_or_fetch_instruments(&cfg.smartapi, layout, args.refresh_instruments).await?;
    let symbols: Vec<Symbol> = if args.symbols.is_empty() {
        universe.symbols.clone()
    } else {
        args.symbols.iter().map(|s| Symbol::new(s.trim())).collect()
    };
    let unknown = missing_symbols(&instruments, universe.exchange, &symbols);
    if !unknown.is_empty() {
        tracing::warn!(
            ?unknown,
            "symbols not in the instrument master will be skipped; check universe.toml"
        );
    }

    let to = args.to.unwrap_or_else(|| MarketClock::ist_date(Utc::now()));
    let from = args.from.unwrap_or(to - Duration::days(args.days));
    let job = CandleJob {
        tf: args.tf,
        exchange: universe.exchange,
        symbols,
        from,
        to,
    };

    api.login().await?;
    let report = fetch_candles(api, layout, &instruments, &job).await?;
    let new_bars: usize = report.updated.iter().map(|(_, n)| n).sum();
    println!(
        "{} tf={} {from}..{to}: {} updated (+{new_bars} bars), {} already up to date, {} failed, {} requests",
        universe.name,
        job.tf,
        report.updated.len(),
        report.up_to_date.len(),
        report.failed.len(),
        report.requests
    );
    for (sym, err) in &report.failed {
        println!("  FAILED {sym}: {err}");
    }
    Ok(if report.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}
