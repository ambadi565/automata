//! `feedcheck`: the M1 exit check, "live 1-min bars match Angel's candles
//! for a full session (±1 tick)".
//!
//! ```text
//! feedcheck record                         # 09:00 → 15:31: record frames, build 1m bars
//! fetch candles --tf 1m --from 2026-10-27 --to 2026-10-27   # after the close
//! feedcheck compare --date 2026-10-27      # pass/fail per symbol
//! feedcheck replay --date 2026-10-27       # rebuild bars from frames (e.g. after changing [bars])
//! ```
//!
//! Recording only reads market data; nothing here can place an order.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, NaiveTime, Utc};
use clap::{Parser, Subcommand};
use feedcheck::compare::compare;
use feedcheck::config::Config;
use feedcheck::{Collector, load_partial, replay};
use fetch::universe::Universe;
use fetch::{load_or_fetch_instruments, missing_symbols};
use saarthi_broker::Broker;
use saarthi_broker::smartapi::{Credentials, SmartApi};
use saarthi_core::{HolidayCalendar, Instrument, MarketClock, SessionTimes, Symbol, Tf};
use saarthi_feed::record::{Recorder, read_frames};
use saarthi_feed::{ExchangeType, FeedAuth, FeedEvent, Mode, market_hours};
use saarthi_store::DataLayout;
use saarthi_store::parquet::read_bars;

#[derive(Parser)]
#[command(
    name = "feedcheck",
    version,
    about = "Record live SmartAPI bars and check them against Angel's candles"
)]
struct Cli {
    #[arg(long, env = "SAARTHI_CONFIG", default_value = "config/engine.toml", global = true)]
    config: PathBuf,
    #[arg(long, env = "SAARTHI_SECRETS", default_value = "config/secrets.env", global = true)]
    secrets: PathBuf,
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Comma-separated subset of the universe (default: all of it).
    #[arg(long, value_delimiter = ',', global = true)]
    symbols: Vec<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Subscribe the universe in Quote mode, record raw frames and build 1m bars.
    Record {
        /// Stop at this IST time.
        #[arg(long, default_value = "15:31")]
        until: String,
    },
    /// Rebuild a day's 1m bars from its recorded frames.
    Replay {
        #[arg(long)]
        date: Option<NaiveDate>,
    },
    /// Compare a day's live 1m bars with Angel's 1m candles from `fetch`.
    Compare {
        #[arg(long)]
        date: Option<NaiveDate>,
        /// Allowed price difference, in ticks.
        #[arg(long, default_value_t = 1)]
        ticks: i64,
        /// Also fail when a minute's volume differs by more than this %.
        #[arg(long)]
        volume_tolerance_pct: Option<f64>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

struct Ctx {
    cfg: Config,
    layout: DataLayout,
    universe: Universe,
    symbols: Vec<Symbol>,
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let cfg = Config::load(&cli.config)?;
    let layout = DataLayout::new(cli.data_dir.clone().unwrap_or_else(|| cfg.paths.data_dir.clone()));
    let universe = Universe::load(&cfg.paths.universe)?;
    let symbols = if cli.symbols.is_empty() {
        universe.symbols.clone()
    } else {
        cli.symbols.iter().map(|s| Symbol::new(s.trim())).collect()
    };
    let ctx = Ctx {
        cfg,
        layout,
        universe,
        symbols,
    };
    let today = MarketClock::ist_date(Utc::now());
    match cli.cmd {
        Cmd::Record { until } => record(&ctx, &cli.secrets, &until).await,
        Cmd::Replay { date } => replay_day(&ctx, date.unwrap_or(today)).await,
        Cmd::Compare {
            date,
            ticks,
            volume_tolerance_pct,
        } => compare_day(&ctx, date.unwrap_or(today), ticks, volume_tolerance_pct).await,
    }
}

async fn instruments(ctx: &Ctx) -> Result<Vec<Instrument>> {
    let all = load_or_fetch_instruments(&ctx.cfg.smartapi, &ctx.layout, false).await?;
    let unknown = missing_symbols(&all, ctx.universe.exchange, &ctx.symbols);
    if !unknown.is_empty() {
        tracing::warn!(?unknown, "symbols not in the instrument master are skipped");
    }
    let wanted: BTreeSet<&Symbol> = ctx.symbols.iter().collect();
    Ok(all
        .into_iter()
        .filter(|i| i.exchange == ctx.universe.exchange && wanted.contains(&i.symbol))
        .collect())
}

fn session_times(cfg: &Config) -> SessionTimes {
    cfg.bars.session
}

async fn record(ctx: &Ctx, secrets: &std::path::Path, until: &str) -> Result<ExitCode> {
    let until = NaiveTime::parse_from_str(until, "%H:%M").context("--until must be HH:MM")?;
    let calendar = HolidayCalendar::load(&ctx.cfg.paths.holidays)?;
    let clock = MarketClock::new(session_times(&ctx.cfg), calendar);
    let date = MarketClock::ist_date(Utc::now());
    if !clock.is_trading_day(date)? {
        bail!("{date} is not a trading day");
    }
    let stop_at = MarketClock::at(date, until);

    let insts = instruments(ctx).await?;
    if insts.is_empty() {
        bail!("no symbols to subscribe");
    }
    let creds = Credentials::load(secrets.exists().then_some(secrets)).context("loading credentials")?;
    let api_key = creds.api_key.clone();
    let api = SmartApi::new(ctx.cfg.smartapi.clone(), creds)?;
    let session = api.login().await?;

    let frames = ctx.layout.live_frames(date);
    let recorder = Recorder::open(&frames).with_context(|| format!("opening {}", frames.display()))?;
    let (feed, mut events) = saarthi_feed::spawn(
        ctx.cfg.feed.clone(),
        FeedAuth::from_session(&session, &api_key),
        market_hours(clock),
        Some(recorder),
    );
    let tokens: Vec<_> = insts
        .iter()
        .map(|i| (ExchangeType::from_exchange(i.exchange), i.token.clone()))
        .collect();
    feed.subscribe(Mode::Quote, tokens).await?;
    let mut col = Collector::new(insts.iter().map(|i| (i.token.clone(), i.symbol.clone())), &ctx.cfg.bars);
    tracing::info!(symbols = insts.len(), frames = %frames.display(), %stop_at, "recording");

    let mut clock_tick = tokio::time::interval(Duration::from_secs(1));
    let mut save_tick = tokio::time::interval(Duration::from_secs(300));
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(FeedEvent::Packet(p)) => col.on_packet(&p),
                Some(FeedEvent::Disconnected { reason }) => {
                    tracing::warn!(%reason, "feed down; next bars will be marked partial");
                    col.on_disconnect();
                }
                Some(FeedEvent::AuthRejected { .. }) => {
                    tracing::warn!("feed refused the session; logging in again");
                    let s = api.login_now().await?;
                    feed.set_auth(FeedAuth::from_session(&s, &api_key)).await?;
                }
                Some(other) => tracing::debug!(?other, "feed event"),
                None => bail!("feed stopped"),
            },
            _ = clock_tick.tick() => {
                let now = Utc::now();
                col.on_time(now);
                if now >= stop_at {
                    break;
                }
            }
            _ = save_tick.tick() => {
                col.save(&ctx.layout, date)?;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted; saving");
                break;
            }
        }
    }
    feed.shutdown().await;
    col.flush();
    let n = col.save(&ctx.layout, date)?;
    let st = feed.status();
    println!(
        "{date}: {} packets, {n} symbols with bars, {} reconnects, {} undecodable frames → {}",
        col.packets,
        st.reconnects,
        st.decode_errors,
        ctx.layout.live_dir(date).display()
    );
    println!(
        "Next, after the close: fetch candles --tf 1m --from {date} --to {date} && feedcheck compare --date {date}"
    );
    Ok(ExitCode::SUCCESS)
}

async fn replay_day(ctx: &Ctx, date: NaiveDate) -> Result<ExitCode> {
    let path = ctx.layout.live_frames(date);
    let frames = read_frames(&path).with_context(|| format!("reading {}", path.display()))?;
    let insts = instruments(ctx).await?;
    let mut col = Collector::new(insts.iter().map(|i| (i.token.clone(), i.symbol.clone())), &ctx.cfg.bars);
    replay(&frames, &mut col);
    let n = col.save(&ctx.layout, date)?;
    println!(
        "{date}: replayed {} frames ({} undecodable), {n} symbols with bars",
        frames.len(),
        col.decode_errors
    );
    Ok(ExitCode::SUCCESS)
}

async fn compare_day(ctx: &Ctx, date: NaiveDate, ticks: i64, vol_tol: Option<f64>) -> Result<ExitCode> {
    let insts = instruments(ctx).await?;
    let partial = load_partial(&ctx.layout, date)?;
    let times = session_times(&ctx.cfg);
    let (from, to) = (MarketClock::at(date, times.open), MarketClock::at(date, times.close));
    let none = BTreeSet::new();
    let (mut pass, mut fail, mut skipped) = (0, 0, 0);
    for inst in &insts {
        let live = read_bars(&ctx.layout.live_bars(date, Tf::M1, &inst.symbol))?;
        let angel = read_bars(&ctx.layout.candles(Tf::M1, &inst.symbol))?;
        if live.is_empty() || angel.iter().all(|b| MarketClock::ist_date(b.ts) != date) {
            tracing::warn!(sym = %inst.symbol, live = live.len(), "no live bars or no Angel candles for the day");
            skipped += 1;
            continue;
        }
        let tol = inst.tick_size.0.max(1) * ticks;
        let excl = partial.get(&inst.symbol).unwrap_or(&none);
        let r = compare(inst.symbol.clone(), &live, &angel, tol, excl, from, to);
        let ok = r.passes(vol_tol);
        if ok {
            pass += 1;
        } else {
            fail += 1;
        }
        println!(
            "{:<4} {:<16} compared {:>3}  excluded {:>2}  missing live {:>2}  missing Angel {:>2}  price diffs {:>2}  vol diff median {:.2}% max {:.2}%",
            if ok { "PASS" } else { "FAIL" },
            inst.symbol.as_str(),
            r.compared,
            r.excluded,
            r.missing_live.len(),
            r.missing_reference.len(),
            r.price_mismatches.len(),
            r.median_volume_diff_pct,
            r.max_volume_diff_pct,
        );
        for m in r.price_mismatches.iter().take(5) {
            println!(
                "       {} {} live {} vs Angel {}",
                m.ts.with_timezone(&saarthi_core::IST).format("%H:%M"),
                m.field,
                m.live,
                m.reference
            );
        }
    }
    println!("{date}: {pass} pass, {fail} fail, {skipped} skipped (±{ticks} tick)");
    Ok(if fail == 0 && pass > 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
