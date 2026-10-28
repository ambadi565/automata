//! Library half of the `fetch` binary, split out so it can be tested against
//! a mocked SmartAPI.

pub mod config;
pub mod universe;

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use saarthi_broker::BrokerError;
use saarthi_broker::smartapi::{SmartApi, SmartApiConfig, historical, instruments};
use saarthi_core::{Exchange, Instrument, MarketClock, SessionTimes, Symbol, Tf};
use saarthi_store::parquet::{merge_bars, read_bars, read_instruments, write_bars, write_instruments};
use saarthi_store::{Coverage, DataLayout, Manifest};

/// Downloads the instrument master and saves it as Parquet.
pub async fn fetch_instruments(cfg: &SmartApiConfig, layout: &DataLayout) -> Result<Vec<Instrument>> {
    let v = instruments::download(cfg)
        .await
        .context("downloading instrument master")?;
    let path = layout.instruments();
    write_instruments(&path, &v).with_context(|| format!("writing {}", path.display()))?;
    tracing::info!(rows = v.len(), path = %path.display(), "instrument master saved");
    Ok(v)
}

/// Loads the saved master, downloading it first if missing or `refresh`.
pub async fn load_or_fetch_instruments(
    cfg: &SmartApiConfig,
    layout: &DataLayout,
    refresh: bool,
) -> Result<Vec<Instrument>> {
    let path = layout.instruments();
    if !refresh && path.exists() {
        return read_instruments(&path).with_context(|| format!("reading {}", path.display()));
    }
    fetch_instruments(cfg, layout).await
}

#[derive(Debug, Clone)]
pub struct CandleJob {
    pub tf: Tf,
    pub exchange: Exchange,
    pub symbols: Vec<Symbol>,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Debug, Default)]
pub struct CandleReport {
    pub up_to_date: Vec<Symbol>,
    pub updated: Vec<(Symbol, usize)>,
    pub failed: Vec<(Symbol, String)>,
    pub requests: usize,
}

/// Downloads candles for every symbol in the job, resuming from what earlier
/// runs already covered. Each request's result is merged and written
/// immediately, so an interrupted run loses at most one request of work.
///
/// Per-symbol failures are collected and the run continues; an auth failure
/// aborts the run since every later request would fail too.
pub async fn fetch_candles(
    api: &SmartApi,
    layout: &DataLayout,
    instruments: &[Instrument],
    job: &CandleJob,
) -> Result<CandleReport> {
    let mut manifest = Manifest::load(&layout.manifest(job.tf))?;
    let mut report = CandleReport::default();
    let times = SessionTimes::default();
    let total = job.symbols.len();

    for (i, sym) in job.symbols.iter().enumerate() {
        let Some(inst) = instruments
            .iter()
            .find(|x| x.exchange == job.exchange && &x.symbol == sym)
        else {
            tracing::warn!(%sym, "not found in instrument master; skipping");
            report.failed.push((sym.clone(), "not in instrument master".into()));
            continue;
        };
        let ranges = Coverage::missing(manifest.get(sym.as_str()), job.from, job.to);
        if ranges.is_empty() {
            report.up_to_date.push(sym.clone());
            continue;
        }
        tracing::info!(%sym, progress = format!("{}/{total}", i + 1), ?ranges, tf = %job.tf, "fetching");

        let path = layout.candles(job.tf, sym);
        let mut bars = read_bars(&path)?;
        let before = bars.len();
        let mut result: Result<(), BrokerError> = Ok(());
        'ranges: for (from, to) in ranges {
            let span = (MarketClock::at(from, times.open), MarketClock::at(to, times.close));
            for (a, b) in historical::chunks(job.tf, span.0, span.1) {
                report.requests += 1;
                match api.candles_chunk(inst, job.tf, a, b).await {
                    Ok(new) => {
                        bars = merge_bars(bars, new);
                        write_bars(&path, &bars)?;
                        manifest.record(sym.as_str(), MarketClock::ist_date(a), MarketClock::ist_date(b))?;
                    }
                    Err(e) => {
                        result = Err(e);
                        break 'ranges;
                    }
                }
            }
        }
        match result {
            Ok(()) => report.updated.push((sym.clone(), bars.len() - before)),
            Err(e @ BrokerError::Auth(_)) => bail!("authentication failed, aborting: {e}"),
            Err(e) => {
                tracing::error!(%sym, error = %e, "fetch failed; rerun to resume");
                report.failed.push((sym.clone(), e.to_string()));
            }
        }
    }
    Ok(report)
}

/// Resolves universe symbols against the master and reports unknown ones.
pub fn missing_symbols(instruments: &[Instrument], exchange: Exchange, symbols: &[Symbol]) -> Vec<Symbol> {
    symbols
        .iter()
        .filter(|s| !instruments.iter().any(|i| i.exchange == exchange && &i.symbol == *s))
        .cloned()
        .collect()
}

pub fn ensure_file(path: &Path, what: &str) -> Result<()> {
    if !path.exists() {
        bail!("{what} not found at {}", path.display());
    }
    Ok(())
}
