//! Replays a [`Dataset`] through one strategy on the simulated exchange,
//! under the `risk.toml` limits.
//!
//! Per bar, in time order across symbols:
//! 1. the exchange plays the bar against working orders (entries placed at
//!    the previous bar's close fill at this bar's open);
//! 2. the symbol's indicator state takes the bar;
//! 3. at the bar's close the strategy manages an open position or emits an
//!    entry signal, which the risk rules size and send as a bracket order
//!    (market entry, SL-M stop, limit target);
//! 4. from the square-off time every position is flattened at market.
//!
//! Nothing a strategy sees at a bar's close can fill on that same bar.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{NaiveDate, TimeDelta};
use saarthi_bars::{BarConfig, IndicatorParams, SymbolState};
use saarthi_broker::backtest::{CostModel, FillKind, SimExchange, SimFill, Slippage};
use saarthi_broker::{OrderKind, OrderRequest, Product};
use saarthi_core::{Exchange, IST, MarketClock, Price, Qty, SessionTimes, Side, Ts};
use saarthi_strategy::{BarCtx, Direction, Exit, ParamSet, Signal, Strategy, registry};
use serde::Serialize;

use crate::config::RiskLimits;
use crate::data::Dataset;

/// Everything fixed across the runs of one backtest.
#[derive(Debug, Clone)]
pub struct Env {
    pub costs: CostModel,
    pub slippage: Slippage,
    pub risk: RiskLimits,
    pub session: SessionTimes,
    /// Base capital for in-sample runs and the first test segment.
    pub capital: Price,
    pub indicators: IndicatorParams,
    pub direction: Direction,
}

/// One run: a strategy with fixed parameters over a date range.
#[derive(Debug, Clone)]
pub struct RunSpec<'a> {
    pub strategy: &'a str,
    pub params: &'a ParamSet,
    /// First day replayed (indicator warm-up starts here).
    pub replay_from: NaiveDate,
    /// First day entries are allowed.
    pub trade_from: NaiveDate,
    /// Last day, inclusive.
    pub to: NaiveDate,
    /// Starting equity (walk-forward carries each test segment's ending
    /// equity into the next).
    pub capital: Price,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trade {
    pub strategy: String,
    pub symbol: String,
    pub side: &'static str,
    pub qty: Qty,
    pub signal_ts: Ts,
    /// The signal's reference price (bar close) — compare with `entry_px`.
    pub signal_px: Price,
    pub entry_ts: Ts,
    pub entry_px: Price,
    pub stop: Price,
    pub target: Option<Price>,
    pub exit_ts: Ts,
    pub exit_px: Price,
    pub exit_reason: String,
    /// P&L before charges.
    pub gross: Price,
    pub charges: Price,
    pub net: Price,
    /// Net P&L in units of the planned risk (entry to stop × qty).
    pub r_multiple: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunResult {
    pub trades: Vec<Trade>,
    /// Account equity at each bar close from `trade_from` on (marked to the
    /// last close of every symbol, net of charges).
    pub equity: Vec<(Ts, Price)>,
    /// Equity at each day's last bar.
    pub daily: Vec<(NaiveDate, Price)>,
    pub capital: Price,
    /// Entry signals the risk rules dropped, by reason.
    pub rejected: HashMap<String, u32>,
    pub bars: usize,
}

struct Pending {
    signal: Signal,
    signal_ts: Ts,
    qty: Qty,
}

struct Open {
    signal: Signal,
    signal_ts: Ts,
    side: Side,
    qty: Qty,
    entry_ts: Ts,
    entry_px: Price,
    charges: Price,
    exit_reason: Option<String>,
    /// A market exit is working; don't send another.
    exit_sent: bool,
}

struct Sym {
    state: SymbolState,
    strat: Box<dyn Strategy>,
    pending: Option<Pending>,
    open: Option<Open>,
    last_close: Option<(Ts, Price)>,
}

fn side_str(s: Side) -> &'static str {
    match s {
        Side::Buy => "long",
        Side::Sell => "short",
    }
}

struct Day {
    date: Option<NaiveDate>,
    start_equity: Price,
    trades: u32,
    halted: bool,
    locked: bool,
    consecutive_losses: u32,
    cooldown_until: Option<Ts>,
}

pub fn run(data: &Dataset, env: &Env, spec: &RunSpec<'_>) -> Result<RunResult> {
    let mut sim = SimExchange::new(env.costs.clone(), env.slippage.clone(), spec.capital);
    let mut syms: Vec<Sym> = Vec::with_capacity(data.symbols.len());
    for s in &data.symbols {
        let bars = BarConfig {
            higher: if data.tf == saarthi_core::Tf::M1 {
                vec![]
            } else {
                vec![data.tf]
            },
            session: env.session,
            ..BarConfig::default()
        };
        let ind = IndicatorParams {
            tf: data.tf,
            ..env.indicators.clone()
        };
        syms.push(Sym {
            state: SymbolState::new(s.symbol.as_str(), bars, ind),
            strat: registry::build(spec.strategy, spec.params)?,
            pending: None,
            open: None,
            last_close: None,
        });
    }
    let tf_len = TimeDelta::minutes(i64::from(data.tf.minutes()));
    let risk = &env.risk;
    let mut out = RunResult {
        capital: spec.capital,
        ..RunResult::default()
    };
    let mut day = Day {
        date: None,
        start_equity: spec.capital,
        trades: 0,
        halted: false,
        locked: false,
        consecutive_losses: 0,
        cooldown_until: None,
    };
    let mut seq = 0u64;
    let range = data.event_range(spec.replay_from, spec.to);
    let events = &data.events[range];
    let mut i = 0;
    while i < events.len() {
        let ts = events[i].0;
        let date = MarketClock::ist_date(ts);
        if day.date != Some(date) {
            // Close anything the previous day left open (data ended before
            // the square-off order could fill).
            for (si, s) in syms.iter_mut().enumerate() {
                s.pending = None;
                if let (Some(_), Some((lts, lpx))) = (&s.open, s.last_close)
                    && let Some(f) = sim.force_close(&data.symbols[si].symbol.0, lpx, lts)
                {
                    close_trade(
                        s,
                        &f,
                        "eod_forced",
                        spec.strategy,
                        &data.symbols[si].symbol.0,
                        &mut out,
                        &mut day,
                    );
                }
                sim.flatten(&data.symbols[si].symbol.0, "eod");
            }
            if let (Some(d), Some(&(_, eq))) = (day.date, out.equity.last())
                && d >= spec.trade_from
            {
                out.daily.push((d, eq));
            }
            sim.prune();
            day = Day {
                date: Some(date),
                start_equity: sim.equity(),
                trades: 0,
                halted: false,
                locked: false,
                consecutive_losses: day.consecutive_losses,
                cooldown_until: None,
            };
        }
        let mut j = i;
        while j < events.len() && events[j].0 == ts {
            j += 1;
        }
        let now = ts + tf_len;
        let local = now.with_timezone(&IST).time();
        let open_at = MarketClock::at(date, env.session.open);
        let trading = date >= spec.trade_from;
        sim.set_time(ts);
        for &(_, si, bi) in &events[i..j] {
            let bar = *data.bar(si, bi);
            let token = data.symbols[si as usize].symbol.0.as_str();
            let s = &mut syms[si as usize];
            for f in sim.on_bar(token, &bar) {
                on_fill(s, &f, spec.strategy, token, &mut out, &mut day, risk);
            }
            s.state.on_history_bar(&bar);
            s.last_close = Some((now, bar.close));
        }
        sim.set_time(now);

        // Daily loss stop / profit lock on marked equity.
        let day_pnl = sim.equity() - day.start_equity;
        let loss_limit = day.start_equity.0 as f64 * risk.max_daily_loss_pct / 100.0;
        if !day.halted && risk.max_daily_loss_pct > 0.0 && (day_pnl.0 as f64) <= -loss_limit {
            day.halted = true;
            for (si, s) in syms.iter_mut().enumerate() {
                s.pending = None;
                if let Some(o) = s.open.as_mut() {
                    o.exit_reason = Some("daily_loss_stop".into());
                    o.exit_sent = true;
                }
                sim.flatten(&data.symbols[si].symbol.0, "dailyloss");
            }
            *out.rejected.entry("daily_loss_halt".into()).or_default() += 1;
        }
        if risk.daily_profit_lock_pct > 0.0
            && (day_pnl.0 as f64) >= day.start_equity.0 as f64 * risk.daily_profit_lock_pct / 100.0
        {
            day.locked = true;
        }

        let squaring = local >= env.session.square_off;
        let mut open_positions = syms.iter().filter(|s| s.open.is_some() || s.pending.is_some()).count();
        let mut gross: i64 = syms
            .iter()
            .filter_map(|s| {
                let px = s.last_close?.1;
                let q = s.open.as_ref().map(|o| o.qty).or(s.pending.as_ref().map(|p| p.qty))?;
                Some(px.0 * i64::from(q))
            })
            .sum();
        for &(_, si, bi) in &events[i..j] {
            let bar = *data.bar(si, bi);
            let token = data.symbols[si as usize].symbol.0.as_str();
            let s = &mut syms[si as usize];
            let position = s.open.as_ref().map(|o| o.side);
            let in_cooldown = day.cooldown_until.is_some_and(|t| now < t);
            let entries_allowed = trading
                && local < env.session.no_new_entries
                && !day.halted
                && !day.locked
                && !in_cooldown
                && day.trades < risk.max_trades_per_day
                && s.pending.is_none();
            let ctx = BarCtx {
                bar,
                now,
                since_open: now - open_at,
                entries_allowed,
                position,
                direction: env.direction,
            };
            let snap = s.state.snapshot().clone();
            let signal = s.strat.on_bar(&snap, &ctx);
            if let Some(o) = s.open.as_mut() {
                if o.exit_sent {
                    continue;
                }
                if squaring {
                    o.exit_reason = Some("square_off".into());
                    o.exit_sent = true;
                    sim.flatten(token, "sqoff");
                } else if let Exit::Close(why) = s.strat.manage(&snap, &ctx) {
                    o.exit_reason = Some(why);
                    o.exit_sent = true;
                    sim.flatten(token, "exit");
                }
                continue;
            }
            if squaring {
                if s.pending.take().is_some() {
                    sim.flatten(token, "sqoff");
                }
                continue;
            }
            let Some(sig) = signal else { continue };
            if !entries_allowed || position.is_some() {
                continue;
            }
            let reject = |out: &mut RunResult, why: &str| *out.rejected.entry(why.to_string()).or_default() += 1;
            if open_positions >= risk.max_open_positions {
                reject(&mut out, "max_open_positions");
                continue;
            }
            if sig.entry < risk.min_price() {
                reject(&mut out, "min_price");
                continue;
            }
            let rps = sig.risk_per_share();
            let risk_amt = day.start_equity.0 as f64 * risk.max_risk_per_trade_pct / 100.0;
            let mut qty = (risk_amt / rps as f64).floor() as i64;
            let exposure_cap = day.start_equity.0 as f64 * risk.max_gross_exposure_x - gross as f64;
            qty = qty.min((exposure_cap / sig.entry.0 as f64).floor() as i64);
            qty = qty.min(i64::from(risk.max_order_qty));
            if qty <= 0 {
                reject(&mut out, "size_zero");
                continue;
            }
            let qty = qty as Qty;
            seq += 1;
            let req = OrderRequest {
                client_order_id: format!("{}-{seq}", spec.strategy),
                symbol: data.symbols[si as usize].symbol.clone(),
                exchange: Exchange::Nse,
                token: token.to_string(),
                side: sig.side,
                qty,
                kind: OrderKind::Market,
                product: Product::Intraday,
                tag: spec.strategy.to_string(),
            };
            sim.place_bracket(req, sig.stop, sig.target)?;
            gross += sig.entry.0 * i64::from(qty);
            open_positions += 1;
            day.trades += 1;
            s.pending = Some(Pending {
                signal: sig,
                signal_ts: now,
                qty,
            });
        }
        if trading {
            out.equity.push((now, sim.equity()));
        }
        out.bars += j - i;
        i = j;
    }
    // Data ended: close what is open at the last price.
    for (si, s) in syms.iter_mut().enumerate() {
        if let (Some(_), Some((lts, lpx))) = (&s.open, s.last_close)
            && let Some(f) = sim.force_close(&data.symbols[si].symbol.0, lpx, lts)
        {
            close_trade(
                s,
                &f,
                "eod_forced",
                spec.strategy,
                &data.symbols[si].symbol.0,
                &mut out,
                &mut day,
            );
        }
    }
    if let Some(d) = day.date
        && d >= spec.trade_from
    {
        let eq = sim.equity();
        if let Some(last) = out.equity.last_mut() {
            last.1 = eq;
        }
        out.daily.push((d, eq));
    }
    Ok(out)
}

fn on_fill(
    s: &mut Sym,
    f: &SimFill,
    strategy: &str,
    symbol: &str,
    out: &mut RunResult,
    day: &mut Day,
    risk: &RiskLimits,
) {
    if s.open.is_none() {
        let Some(p) = s.pending.take() else {
            return;
        };
        s.open = Some(Open {
            side: p.signal.side,
            qty: f.qty,
            entry_ts: f.ts,
            entry_px: f.price,
            charges: f.charges.total(),
            signal: p.signal,
            signal_ts: p.signal_ts,
            exit_reason: None,
            exit_sent: false,
        });
        return;
    }
    let reason = match f.kind {
        FillKind::Stop => "stop",
        FillKind::Limit => "target",
        FillKind::Market => "market",
    };
    close_trade(s, f, reason, strategy, symbol, out, day);
    if day.consecutive_losses >= risk.max_consecutive_losses && risk.max_consecutive_losses > 0 {
        day.cooldown_until = Some(f.ts + TimeDelta::minutes(risk.consecutive_loss_cooldown_min));
        day.consecutive_losses = 0;
    }
}

fn close_trade(s: &mut Sym, f: &SimFill, kind: &str, strategy: &str, symbol: &str, out: &mut RunResult, day: &mut Day) {
    let Some(o) = s.open.take() else { return };
    let q = i64::from(o.qty);
    let gross = match o.side {
        Side::Buy => (f.price.0 - o.entry_px.0) * q,
        Side::Sell => (o.entry_px.0 - f.price.0) * q,
    };
    let charges = o.charges + f.charges.total();
    let net = gross - charges.0;
    let planned = (o.signal.risk_per_share() * q).max(1);
    let exit_reason = match (kind, o.exit_reason) {
        ("stop" | "target", _) => kind.to_string(),
        (_, Some(r)) => r,
        (k, None) => k.to_string(),
    };
    if net < 0 {
        day.consecutive_losses += 1;
    } else {
        day.consecutive_losses = 0;
    }
    out.trades.push(Trade {
        strategy: strategy.to_string(),
        symbol: symbol.to_string(),
        side: side_str(o.side),
        qty: o.qty,
        signal_ts: o.signal_ts,
        signal_px: o.signal.entry,
        entry_ts: o.entry_ts,
        entry_px: o.entry_px,
        stop: o.signal.stop,
        target: o.signal.target,
        exit_ts: f.ts,
        exit_px: f.price,
        exit_reason,
        gross: Price(gross),
        charges,
        net: Price(net),
        r_multiple: net as f64 / planned as f64,
        reason: o.signal.reason,
    });
}
