//! `report`: the hourly update and the end-of-day report.
//!
//! ```text
//! report hourly            # 10:15 … 14:15 (saarthi-report.timer)
//! report eod               # ~15:40 (after the strategist's EOD journal)
//! report eod --date 2026-10-28 --print   # rebuild an old day, print instead of sending
//! ```
//!
//! Without Telegram credentials the report is printed and archived only.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use clap::{Parser, Subcommand};
use engine::config::Config;
use engine::{AccountFiles, account_id, secret};
use saarthi_core::MarketClock;
use saarthi_notify::Telegram;
use saarthi_store::{DataLayout, Db};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "report", version, about = "Saarthi hourly and end-of-day reports")]
struct Cli {
    #[arg(long, env = "SAARTHI_CONFIG", default_value = "config/engine.toml", global = true)]
    config: PathBuf,
    #[arg(long, env = "SAARTHI_SECRETS", default_value = "config/secrets.env", global = true)]
    secrets: PathBuf,
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    account: Option<String>,
    /// Print instead of sending to Telegram.
    #[arg(long, global = true)]
    print: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Live update from the running engine.
    Hourly,
    /// End-of-day report from the store; archived as data/reports/<date>.html.
    Eod {
        #[arg(long)]
        date: Option<NaiveDate>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    match run(Cli::parse()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn secrets(p: &Path) -> Option<&Path> {
    p.exists().then_some(p)
}

/// Sends to Telegram; `Ok(true)` when it went out, `Ok(false)` when printed.
async fn deliver(cli: &Cli, text: &str, png: Option<Vec<u8>>, doc: Option<(&str, Vec<u8>)>) -> Result<bool> {
    let tg = if cli.print {
        None
    } else {
        Telegram::from_secrets(secrets(&cli.secrets))
    };
    let Some(tg) = tg else {
        if !cli.print {
            tracing::warn!("TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID not set; printing instead");
        }
        println!("{text}");
        return Ok(false);
    };
    tg.send_text(text).await.context("sending the report")?;
    if let Some(p) = png
        && let Err(e) = tg.send_photo(p, "Intraday equity (net of charges)").await
    {
        tracing::warn!(error = %e, "chart not sent");
    }
    if let Some((name, bytes)) = doc
        && let Err(e) = tg.send_document(name, bytes, "text/html", "Full report").await
    {
        tracing::warn!(error = %e, "HTML report not sent");
    }
    Ok(true)
}

/// Counts a delivered report toward the M5 exit check.
fn record(db_path: &Path, date: NaiveDate, f: impl FnOnce(&mut engine::Delivered)) -> Result<()> {
    let mut db = Db::open(db_path)?;
    let key = engine::delivered_key(date);
    let mut d: engine::Delivered = db.kv_get(&key)?.unwrap_or_default();
    f(&mut d);
    db.write(|w| w.kv_put(&key, &d))?;
    Ok(())
}

async fn get(c: &reqwest::Client, base: &str, token: &str, path: &str) -> Result<Value> {
    let r = c
        .get(format!("{base}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("engine unreachable: {}", e.without_url()))?;
    let status = r.status();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    anyhow::ensure!(status.is_success(), "engine said {status}: {v}");
    Ok(v)
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let cfg = Config::load(&cli.config)?;
    let layout = DataLayout::new(cli.data_dir.clone().unwrap_or_else(|| cfg.paths.data_dir.clone()));
    let acct = AccountFiles::new(&layout, &account_id(cli.account.as_deref(), &cli.secrets), "paper");
    let journal = layout.root().join("journal");
    let now = Utc::now();
    match &cli.cmd {
        Cmd::Hourly => {
            let date = MarketClock::ist_date(now);
            let token = secret(secrets(&cli.secrets), "CONTROL_API_TOKEN")
                .context("CONTROL_API_TOKEN is not set; the hourly report reads the engine's control API")?;
            let base = format!("http://{}", cfg.engine.control_addr);
            let c = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
            let text = match (
                get(&c, &base, &token, "/state").await,
                get(&c, &base, &token, "/pnl").await,
                get(&c, &base, &token, "/positions").await,
            ) {
                (Ok(s), Ok(p), Ok(pos)) => {
                    report::hourly_text(now, &s, &p, &pos, report::latest_note(&journal, date).as_deref())
                }
                (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => {
                    format!("🚨 Saarthi hourly: cannot reach the engine ({e:#}). Check `journalctl -u saarthi-engine`.")
                }
            };
            let engine_up = !text.starts_with("🚨");
            if deliver(&cli, &text, None, None).await? && engine_up {
                record(&acct.db(), date, |d| d.hourly += 1)?;
            }
        }
        Cmd::Eod { date } => {
            let date = date.unwrap_or_else(|| MarketClock::ist_date(now));
            let db = Db::open(&acct.db())?;
            let e = report::load_eod(&db, date, &journal)?;
            let html = report::eod_html(&e);
            let dir = layout.root().join("reports");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{date}.html"));
            std::fs::write(&path, &html).with_context(|| format!("writing {}", path.display()))?;
            tracing::info!(path = %path.display(), "EOD report archived");
            let png = report::equity_png(&report::equity_points(&e.trades), &format!("Saarthi {date}"));
            let name = format!("saarthi-{date}.html");
            drop(db);
            if deliver(&cli, &report::eod_text(&e), png, Some((&name, html.into_bytes()))).await? {
                record(&acct.db(), date, |d| d.eod = true)?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
