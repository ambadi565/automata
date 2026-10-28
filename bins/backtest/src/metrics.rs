//! Performance statistics from trades and an equity curve.

use chrono::NaiveDate;
use saarthi_core::{Price, Ts};
use serde::Serialize;

use crate::runner::Trade;

/// Profit factor is reported as this when there are no losing trades, so it
/// stays a finite JSON number.
pub const PF_CAP: f64 = 99.0;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Metrics {
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate_pct: f64,
    /// Rupees.
    pub gross_pnl: f64,
    pub charges: f64,
    pub net_pnl: f64,
    /// Sum of net wins / |sum of net losses| (net of charges).
    pub profit_factor: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    /// Average net P&L per trade.
    pub expectancy: f64,
    pub avg_r: f64,
    pub return_pct: f64,
    /// Peak-to-trough on the bar-level equity curve, % of the peak.
    pub max_drawdown_pct: f64,
    pub max_drawdown_inr: f64,
    /// Annualised (√252) Sharpe of daily returns, risk-free rate 0.
    pub sharpe: f64,
    pub days: usize,
    pub trades_per_day: f64,
    /// Average entry slippage vs the signal price, in basis points (positive
    /// = worse).
    pub avg_entry_slippage_bps: f64,
    /// Charges as % of gross profit of winners — how much the costs eat.
    pub charges_pct_of_gross_wins: f64,
}

fn rupees(p: i64) -> f64 {
    p as f64 / 100.0
}

/// Max drawdown `(pct of peak, absolute paise)` of an equity series.
pub fn max_drawdown(equity: impl IntoIterator<Item = Price>) -> (f64, i64) {
    let mut peak: Option<i64> = None;
    let (mut pct, mut abs) = (0.0f64, 0i64);
    for e in equity {
        let p = *peak.get_or_insert(e.0);
        if e.0 > p {
            peak = Some(e.0);
            continue;
        }
        let dd = p - e.0;
        abs = abs.max(dd);
        if p > 0 {
            pct = pct.max(dd as f64 * 100.0 / p as f64);
        }
    }
    (pct, abs)
}

/// Annualised Sharpe of day-over-day returns of `daily` equity, starting
/// from `start`.
pub fn sharpe(start: Price, daily: &[(NaiveDate, Price)]) -> f64 {
    let mut prev = start.0 as f64;
    let mut rets = Vec::with_capacity(daily.len());
    for &(_, e) in daily {
        if prev > 0.0 {
            rets.push(e.0 as f64 / prev - 1.0);
        }
        prev = e.0 as f64;
    }
    if rets.len() < 2 {
        return 0.0;
    }
    let n = rets.len() as f64;
    let mean = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let sd = var.sqrt();
    if sd <= 0.0 { 0.0 } else { mean / sd * 252f64.sqrt() }
}

pub fn compute(trades: &[Trade], capital: Price, equity: &[(Ts, Price)], daily: &[(NaiveDate, Price)]) -> Metrics {
    let mut m = Metrics {
        trades: trades.len(),
        days: daily.len(),
        ..Metrics::default()
    };
    let (mut win_sum, mut loss_sum, mut gross_wins) = (0i64, 0i64, 0i64);
    let (mut gross, mut charges, mut r_sum, mut slip_sum) = (0i64, 0i64, 0.0, 0.0);
    for t in trades {
        gross += t.gross.0;
        charges += t.charges.0;
        r_sum += t.r_multiple;
        if t.net.0 > 0 {
            m.wins += 1;
            win_sum += t.net.0;
        } else {
            m.losses += 1;
            loss_sum += t.net.0;
        }
        if t.gross.0 > 0 {
            gross_wins += t.gross.0;
        }
        let worse = match t.side {
            "long" => t.entry_px.0 - t.signal_px.0,
            _ => t.signal_px.0 - t.entry_px.0,
        };
        if t.signal_px.0 > 0 {
            slip_sum += worse as f64 * 1e4 / t.signal_px.0 as f64;
        }
    }
    let n = trades.len().max(1) as f64;
    m.gross_pnl = rupees(gross);
    m.charges = rupees(charges);
    m.net_pnl = rupees(gross - charges);
    m.win_rate_pct = if trades.is_empty() {
        0.0
    } else {
        m.wins as f64 * 100.0 / n
    };
    m.profit_factor = if loss_sum == 0 {
        if win_sum > 0 { PF_CAP } else { 0.0 }
    } else {
        (win_sum as f64 / -loss_sum as f64).min(PF_CAP)
    };
    m.avg_win = if m.wins > 0 {
        rupees(win_sum) / m.wins as f64
    } else {
        0.0
    };
    m.avg_loss = if m.losses > 0 {
        rupees(loss_sum) / m.losses as f64
    } else {
        0.0
    };
    m.expectancy = if trades.is_empty() { 0.0 } else { m.net_pnl / n };
    m.avg_r = if trades.is_empty() { 0.0 } else { r_sum / n };
    m.avg_entry_slippage_bps = if trades.is_empty() { 0.0 } else { slip_sum / n };
    m.charges_pct_of_gross_wins = if gross_wins > 0 {
        charges as f64 * 100.0 / gross_wins as f64
    } else {
        0.0
    };
    let end = equity.last().map_or(capital, |e| e.1);
    m.return_pct = if capital.0 > 0 {
        (end.0 - capital.0) as f64 * 100.0 / capital.0 as f64
    } else {
        0.0
    };
    let (dd_pct, dd_abs) = max_drawdown(std::iter::once(capital).chain(equity.iter().map(|e| e.1)));
    m.max_drawdown_pct = dd_pct;
    m.max_drawdown_inr = rupees(dd_abs);
    m.sharpe = sharpe(capital, daily);
    m.trades_per_day = if daily.is_empty() {
        0.0
    } else {
        trades.len() as f64 / daily.len() as f64
    };
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawdown_from_peak() {
        let eq = [100, 120, 90, 130, 117].map(Price);
        let (pct, abs) = max_drawdown(eq);
        assert_eq!(abs, 30);
        assert!((pct - 25.0).abs() < 1e-9);
        assert_eq!(max_drawdown([Price(1), Price(2), Price(3)]), (0.0, 0));
    }

    #[test]
    fn sharpe_of_constant_returns_is_zero_risk() {
        let d = |i| NaiveDate::from_ymd_opt(2026, 1, i).unwrap();
        let flat = [(d(1), Price(100)), (d(2), Price(100)), (d(3), Price(100))];
        assert!(sharpe(Price(100), &flat).abs() < 1e-12);
        let up_down = [(d(1), Price(110)), (d(2), Price(100)), (d(3), Price(110))];
        assert!(sharpe(Price(100), &up_down) > 0.0);
    }
}
