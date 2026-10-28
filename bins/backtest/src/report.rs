//! Writes a backtest's results: CSVs, JSON summary, an SVG equity curve and
//! a Markdown report.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use saarthi_core::{IST, Price, Ts};
use serde::Serialize;

use crate::metrics::Metrics;
use crate::runner::{RunResult, Trade};
use crate::walkforward::{Gate, WalkForwardResult};

fn write(path: &Path, s: &str) -> Result<()> {
    saarthi_store::atomic_write(path, |f| {
        use std::io::Write;
        f.write_all(s.as_bytes())
            .map_err(|e| saarthi_store::StoreError::io(path, e))
    })
    .with_context(|| format!("writing {}", path.display()))
}

fn ist(ts: Ts) -> String {
    ts.with_timezone(&IST).format("%Y-%m-%d %H:%M").to_string()
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn trades_csv(trades: &[Trade]) -> String {
    let mut s = String::from(
        "strategy,symbol,side,qty,signal_time,signal_px,entry_time,entry_px,stop,target,exit_time,exit_px,exit_reason,gross,charges,net,r_multiple,reason\n",
    );
    for t in trades {
        let _ = writeln!(
            s,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{:.3},{}",
            t.strategy,
            csv_field(&t.symbol),
            t.side,
            t.qty,
            ist(t.signal_ts),
            t.signal_px,
            ist(t.entry_ts),
            t.entry_px,
            t.stop,
            t.target.map(|p| p.to_string()).unwrap_or_default(),
            ist(t.exit_ts),
            t.exit_px,
            csv_field(&t.exit_reason),
            t.gross,
            t.charges,
            t.net,
            t.r_multiple,
            csv_field(&t.reason),
        );
    }
    s
}

pub fn equity_csv(equity: &[(Ts, Price)]) -> String {
    let mut s = String::from("time,equity\n");
    for (t, e) in equity {
        let _ = writeln!(s, "{},{}", ist(*t), e);
    }
    s
}

/// A dependency-free SVG line chart of daily equity with the drawdown
/// shaded underneath. Light/dark aware.
pub fn equity_svg(title: &str, capital: Price, daily: &[(NaiveDate, Price)]) -> String {
    let (w, h, pad_l, pad_r, pad_t, pad_b) = (900.0, 360.0, 70.0, 20.0, 36.0, 40.0);
    let mut pts: Vec<(Option<NaiveDate>, f64)> = vec![(None, capital.as_rupees_f64())];
    pts.extend(daily.iter().map(|(d, e)| (Some(*d), e.as_rupees_f64())));
    let lo = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let hi = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
    let span = (hi - lo).max(1.0);
    let (lo, hi) = (lo - span * 0.05, hi + span * 0.05);
    let n = (pts.len() - 1).max(1) as f64;
    let x = |i: usize| pad_l + (w - pad_l - pad_r) * i as f64 / n;
    let y = |v: f64| pad_t + (h - pad_t - pad_b) * (hi - v) / (hi - lo);
    let mut line = String::new();
    let mut peak_line = String::new();
    let mut peak = f64::NEG_INFINITY;
    for (i, (_, v)) in pts.iter().enumerate() {
        peak = peak.max(*v);
        let _ = write!(line, "{}{:.1},{:.1}", if i == 0 { "M" } else { " L" }, x(i), y(*v));
        let _ = write!(
            peak_line,
            "{}{:.1},{:.1}",
            if i == 0 { "M" } else { " L" },
            x(i),
            y(peak)
        );
    }
    // Drawdown area: between running peak and equity.
    let mut area = peak_line.clone();
    for (i, (_, v)) in pts.iter().enumerate().rev() {
        let _ = write!(area, " L{:.1},{:.1}", x(i), y(*v));
    }
    area.push_str(" Z");
    let mut s = String::new();
    let _ = write!(
        s,
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" font-family="system-ui, sans-serif" font-size="12">
<style>
  .bg {{ fill: #ffffff }} .fg {{ fill: #1f2328 }} .muted {{ fill: #656d76 }} .grid {{ stroke: #d0d7de }}
  .eq {{ stroke: #0969da }} .dd {{ fill: #cf222e; fill-opacity: .18 }} .cap {{ stroke: #656d76 }}
  @media (prefers-color-scheme: dark) {{
    .bg {{ fill: #0d1117 }} .fg {{ fill: #e6edf3 }} .muted {{ fill: #8d96a0 }} .grid {{ stroke: #30363d }}
    .eq {{ stroke: #4493f8 }} .dd {{ fill: #f85149; fill-opacity: .25 }} .cap {{ stroke: #8d96a0 }}
  }}
</style>
<rect class="bg" width="{w}" height="{h}"/>
<text class="fg" x="{pad_l}" y="22" font-size="14" font-weight="600">{}</text>
"##,
        xml(title)
    );
    for k in 0..=4 {
        let v = lo + (hi - lo) * f64::from(k) / 4.0;
        let _ = writeln!(
            s,
            r#"<line class="grid" x1="{pad_l}" x2="{}" y1="{:.1}" y2="{:.1}" stroke-width="1"/><text class="muted" x="{}" y="{:.1}" text-anchor="end">₹{:.0}</text>"#,
            w - pad_r,
            y(v),
            y(v),
            pad_l - 6.0,
            y(v) + 4.0,
            v
        );
    }
    let cap_y = y(capital.as_rupees_f64());
    let _ = writeln!(
        s,
        r#"<line class="cap" x1="{pad_l}" x2="{}" y1="{cap_y:.1}" y2="{cap_y:.1}" stroke-dasharray="4 4" stroke-width="1"/>"#,
        w - pad_r
    );
    let _ = writeln!(s, r#"<path class="dd" d="{area}"/>"#);
    let _ = writeln!(s, r#"<path class="eq" d="{line}" fill="none" stroke-width="2"/>"#);
    if let (Some((Some(d0), _)), Some((Some(d1), _))) = (pts.get(1), pts.last()) {
        let _ = writeln!(
            s,
            r#"<text class="muted" x="{pad_l}" y="{}">{d0}</text><text class="muted" x="{}" y="{}" text-anchor="end">{d1}</text>"#,
            h - 14.0,
            w - pad_r,
            h - 14.0
        );
    }
    s.push_str("</svg>\n");
    s
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn metrics_table(rows: &[(&str, &Metrics)]) -> String {
    let mut s = String::from(
        "| | Trades | Win % | PF | Net ₹ | Charges ₹ | Return % | Max DD % | Sharpe | Avg R | Entry slip bps |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for (name, m) in rows {
        let _ = writeln!(
            s,
            "| {name} | {} | {:.1} | {:.2} | {:.0} | {:.0} | {:.2} | {:.2} | {:.2} | {:.2} | {:.1} |",
            m.trades,
            m.win_rate_pct,
            m.profit_factor,
            m.net_pnl,
            m.charges,
            m.return_pct,
            m.max_drawdown_pct,
            m.sharpe,
            m.avg_r,
            m.avg_entry_slippage_bps
        );
    }
    s
}

pub fn gate_md(g: &Gate) -> String {
    let mut s = format!(
        "**Gate: {}**\n\n| Check | Value | Needs | |\n|---|---:|---:|---|\n",
        if g.pass { "PASS" } else { "FAIL" }
    );
    for c in &g.checks {
        let op = match c.name {
            "oos_max_drawdown_pct" => "<",
            "oos_profit_factor" => ">",
            _ => "≥",
        };
        let _ = writeln!(
            s,
            "| {} | {:.2} | {op} {:.2} | {} |",
            c.name,
            c.value,
            c.threshold,
            if c.pass { "pass" } else { "FAIL" }
        );
    }
    s
}

#[derive(Serialize)]
struct Summary<'a> {
    strategy: &'a str,
    mode: &'a str,
    data_note: &'a str,
    #[serde(flatten)]
    wf: &'a WalkForwardResult,
}

/// Writes one strategy's walk-forward results into `dir`.
pub fn write_walk_forward(dir: &Path, r: &WalkForwardResult, capital: Price, data_note: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    write(&dir.join("trades.csv"), &trades_csv(&r.oos_trades))?;
    write(&dir.join("equity.csv"), &equity_csv(&r.oos_equity))?;
    write(
        &dir.join("equity.svg"),
        &equity_svg(&format!("{} — out-of-sample equity", r.strategy), capital, &r.oos_daily),
    )?;
    let summary = Summary {
        strategy: &r.strategy,
        mode: "walk_forward",
        data_note,
        wf: r,
    };
    write(&dir.join("summary.json"), &serde_json::to_string_pretty(&summary)?)?;

    let mut w = String::from(
        "window,train_from,train_to,test_from,test_to,params,train_trades,train_pf,train_return_pct,test_trades,test_pf,test_net,test_max_dd_pct,neighbours_profitable,neighbours,fallback_defaults\n",
    );
    for x in &r.windows {
        let _ = writeln!(
            w,
            "{},{},{},{},{},{},{},{:.3},{:.3},{},{:.3},{:.2},{:.3},{},{},{}",
            x.window.index,
            x.window.train_from,
            x.window.train_to,
            x.window.test_from,
            x.window.test_to,
            csv_field(&x.params.to_string()),
            x.train.trades,
            x.train.profit_factor,
            x.train.return_pct,
            x.test.trades,
            x.test.profit_factor,
            x.test.net_pnl,
            x.test.max_drawdown_pct,
            x.neighbours_profitable,
            x.neighbours,
            x.fallback_defaults
        );
    }
    write(&dir.join("windows.csv"), &w)?;

    let mut md = format!("# Backtest: {}\n\n", r.strategy);
    if !data_note.is_empty() {
        let _ = writeln!(md, "> {data_note}\n");
    }
    let _ = writeln!(md, "Walk-forward, {} windows. Out-of-sample only:\n", r.windows.len());
    md.push_str(&metrics_table(&[("OOS", &r.oos)]));
    let _ = writeln!(
        md,
        "\nParameter stability: {:.0} % of the chosen sets' grid neighbours were profitable in-sample; {} distinct sets chosen over {} windows; {} of {} test windows profitable.\n",
        r.neighbour_profitable_share * 100.0,
        r.distinct_param_sets,
        r.windows.len(),
        r.oos_profitable_windows,
        r.windows.len()
    );
    md.push_str(&gate_md(&r.gate));
    md.push_str("\n![equity](equity.svg)\n\n## Windows\n\n| # | Test | Params | IS trades | IS PF | OOS trades | OOS PF | OOS net ₹ |\n|---:|---|---|---:|---:|---:|---:|---:|\n");
    for x in &r.windows {
        let _ = writeln!(
            md,
            "| {} | {} → {} | `{}`{} | {} | {:.2} | {} | {:.2} | {:.0} |",
            x.window.index,
            x.window.test_from,
            x.window.test_to,
            x.params,
            if x.fallback_defaults { " (defaults)" } else { "" },
            x.train.trades,
            x.train.profit_factor,
            x.test.trades,
            x.test.profit_factor,
            x.test.net_pnl
        );
    }
    write(&dir.join("report.md"), &md)?;
    Ok(())
}

/// The cross-strategy summary at the top of the output directory.
pub fn write_overview(dir: &Path, results: &[WalkForwardResult], header: &str) -> Result<String> {
    let mut md = format!("# Backtest overview\n\n{header}\n\n");
    let rows: Vec<(&str, &Metrics)> = results.iter().map(|r| (r.strategy.as_str(), &r.oos)).collect();
    md.push_str("Out-of-sample, after charges and slippage:\n\n");
    md.push_str(&metrics_table(&rows));
    md.push_str("\n| Strategy | Gate | Failed checks |\n|---|---|---|\n");
    for r in results {
        let failed: Vec<&str> = r.gate.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect();
        let _ = writeln!(
            md,
            "| {} | {} | {} |",
            r.strategy,
            if r.gate.pass { "PASS" } else { "FAIL" },
            if failed.is_empty() {
                "—".to_string()
            } else {
                failed.join(", ")
            }
        );
    }
    write(&dir.join("overview.md"), &md)?;
    Ok(md)
}

/// A single in-sample run (no walk-forward): no gate verdict.
pub fn write_single(dir: &Path, strategy: &str, params: &str, m: &Metrics, r: &RunResult) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    write(&dir.join("trades.csv"), &trades_csv(&r.trades))?;
    write(&dir.join("equity.csv"), &equity_csv(&r.equity))?;
    write(
        &dir.join("equity.svg"),
        &equity_svg(&format!("{strategy} — single run (in-sample)"), r.capital, &r.daily),
    )?;
    write(
        &dir.join("summary.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "strategy": strategy,
            "mode": "single",
            "params": params,
            "metrics": m,
            "gate": "not evaluated: a single run is in-sample; use --walk-forward",
        }))?,
    )?;
    let mut md =
        format!("# Backtest: {strategy} (single run, `{params}`)\n\nIn-sample only; the gate needs walk-forward.\n\n");
    md.push_str(&metrics_table(&[("all", m)]));
    md.push_str("\n![equity](equity.svg)\n");
    write(&dir.join("report.md"), &md)?;
    Ok(())
}
