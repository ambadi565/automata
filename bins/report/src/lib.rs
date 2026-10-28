//! Reports (ARCHITECTURE §5.9).
//!
//! * **Hourly** (10:15 … 14:15): P&L, trades, win rate, open positions with
//!   stops, loss budget used, engine and feed health, and the strategist's
//!   2–3 line note. Read live from the engine's control API.
//! * **EOD** (~15:40): equity curve, trade table, per-strategy stats, charges,
//!   slippage vs signal, the incident log (should be empty), the strategist's
//!   journal. Read from SQLite, so it works after the engine has exited and
//!   even when the day ended in a halt. Saved as
//!   `data/reports/YYYY-MM-DD.html`, sent to Telegram as text + PNG + the HTML.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use chrono::{NaiveDate, TimeDelta};
use engine::DaySummary;
use saarthi_core::{MarketClock, Price, Ts};
use saarthi_oms::{Trade, TradeState};
use saarthi_store::{Db, EventRow};
use serde::Serialize;
use serde_json::Value;

/// Everything the EOD report shows.
#[derive(Debug, Clone, Default)]
pub struct Eod {
    pub date: NaiveDate,
    pub summary: Option<DaySummary>,
    pub trades: Vec<Trade>,
    pub events: Vec<EventRow>,
    pub journal: Option<String>,
    pub recent: Vec<(NaiveDate, DaySummary)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StrategyStats {
    pub trades: u32,
    pub wins: u32,
    pub net: Price,
    pub charges: Price,
    /// Mean adverse entry slippage per share vs the signal price.
    pub avg_slippage: Price,
}

/// IST midnight of `date` and of the next day, as UTC RFC 3339.
fn day_bounds(date: NaiveDate) -> (String, String) {
    let start = MarketClock::at(date, chrono::NaiveTime::MIN);
    (start.to_rfc3339(), (start + TimeDelta::days(1)).to_rfc3339())
}

pub fn load_eod(db: &Db, date: NaiveDate, journal_dir: &Path) -> anyhow::Result<Eod> {
    let (from, to) = day_bounds(date);
    let mut recent: Vec<(NaiveDate, DaySummary)> = db.days()?;
    recent.retain(|(d, _)| *d <= date);
    let keep = recent.len().saturating_sub(20);
    recent.drain(..keep);
    Ok(Eod {
        date,
        summary: db.day(date)?,
        trades: db.trades_on(date)?,
        events: db.events_between(&from, &to)?,
        journal: std::fs::read_to_string(journal_dir.join(format!("{date}.md"))).ok(),
        recent,
    })
}

/// Entry slippage per share against the signal (positive = worse).
pub fn slippage(t: &Trade) -> Option<Price> {
    t.avg_entry().map(|a| Price(t.side.sign() * (a.0 - t.signal_px.0)))
}

pub fn per_strategy(trades: &[Trade]) -> BTreeMap<String, StrategyStats> {
    let mut m: BTreeMap<String, (StrategyStats, i64, i64)> = BTreeMap::new();
    for t in trades.iter().filter(|t| t.entry_qty > 0) {
        let (s, slip_sum, n) = m.entry(t.strategy.clone()).or_default();
        let net = t.net(None);
        s.trades += 1;
        s.wins += u32::from(net.0 > 0);
        s.net = s.net + net;
        s.charges = s.charges + t.charges;
        if let Some(p) = slippage(t) {
            *slip_sum += p.0;
            *n += 1;
        }
    }
    m.into_iter()
        .map(|(k, (mut s, sum, n))| {
            if n > 0 {
                s.avg_slippage = Price(sum / n);
            }
            (k, s)
        })
        .collect()
}

/// Cumulative net P&L at each trade close, starting from zero at the first
/// open.
pub fn equity_points(trades: &[Trade]) -> Vec<(Ts, i64)> {
    let mut closed: Vec<&Trade> = trades
        .iter()
        .filter(|t| t.state == TradeState::Closed && t.closed_at.is_some())
        .collect();
    closed.sort_by_key(|t| t.closed_at);
    let mut out = Vec::new();
    if let Some(first) = closed.iter().filter_map(|t| t.opened_at).min() {
        out.push((first, 0));
    }
    let mut cum = 0i64;
    for t in closed {
        cum += t.net(None).0;
        out.push((t.closed_at.unwrap_or_default(), cum));
    }
    out
}

/// Warnings and critical alerts recorded during the day.
pub fn incidents(e: &Eod) -> Vec<(String, String, String)> {
    e.events
        .iter()
        .filter(|ev| ev.kind == "engine_alert" || ev.kind == "alert")
        .filter_map(|ev| {
            let v: Value = serde_json::from_str(&ev.detail).ok()?;
            let (sev, msg) = if ev.kind == "engine_alert" {
                (v["severity"].as_str()?.to_string(), v["message"].as_str()?.to_string())
            } else {
                let a = &v["Alert"];
                (
                    a["severity"].as_str().unwrap_or("warning").to_lowercase(),
                    a["message"].as_str().unwrap_or("").to_string(),
                )
            };
            (sev != "info").then(|| (ist_hm(&ev.ts), sev, msg))
        })
        .collect()
}

fn ist_hm(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| t.with_timezone(&saarthi_core::IST).format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| rfc3339.to_string())
}

fn hm(t: Option<Ts>) -> String {
    t.map(|t| t.with_timezone(&saarthi_core::IST).format("%H:%M").to_string())
        .unwrap_or_else(|| "—".into())
}

fn opt(p: Option<Price>) -> String {
    p.map(|p| p.to_string()).unwrap_or_else(|| "—".into())
}

/// The Telegram summary for the day.
pub fn eod_text(e: &Eod) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "📊 Saarthi EOD {}", e.date);
    match &e.summary {
        None => {
            let _ = writeln!(s, "No session recorded today (holiday, or the engine did not run).");
        }
        Some(d) => {
            let closed = d.wins + d.losses;
            let _ = writeln!(
                s,
                "Net ₹{} (gross ₹{}, charges ₹{})\nTrades {} | W {} L {}{}",
                d.net,
                d.gross,
                d.charges,
                d.trades,
                d.wins,
                d.losses,
                if closed > 0 {
                    format!(" | win rate {:.0}%", 100.0 * f64::from(d.wins) / f64::from(closed))
                } else {
                    String::new()
                }
            );
            let _ = writeln!(s, "Worst intraday ₹{}", d.worst_pnl);
            let _ = writeln!(
                s,
                "Flat after square-off: {}{}",
                if d.flat_after_square_off { "yes ✅" } else { "NO ❌" },
                d.flat_confirmed_at
                    .map(|t| format!(" ({})", hm(Some(t))))
                    .unwrap_or_default()
            );
            if let Some(h) = &d.halted {
                let _ = writeln!(s, "Halted: {h}");
            }
            if d.restarts > 0 {
                let _ = writeln!(s, "Engine restarts: {}", d.restarts);
            }
        }
    }
    let per = per_strategy(&e.trades);
    if !per.is_empty() {
        let _ = writeln!(s, "\nBy strategy:");
        for (k, v) in &per {
            let _ = writeln!(
                s,
                "• {k}: {} trades, {} wins, net ₹{}, slippage ₹{}/sh",
                v.trades, v.wins, v.net, v.avg_slippage
            );
        }
    }
    let inc = incidents(e);
    let _ = writeln!(
        s,
        "\nIncidents: {}",
        if inc.is_empty() {
            "none".to_string()
        } else {
            inc.len().to_string()
        }
    );
    for (t, sev, m) in inc.iter().take(5) {
        let _ = writeln!(s, "• {t} {sev}: {}", m.chars().take(160).collect::<String>());
    }
    if let Some(j) = &e.journal
        && let Some(seed) = section(j, "Tomorrow")
    {
        let _ = writeln!(s, "\nTomorrow: {}", seed.chars().take(600).collect::<String>());
    }
    s
}

/// Body of the markdown section whose heading starts with `name`.
fn section(md: &str, name: &str) -> Option<String> {
    let mut out = String::new();
    let mut inside = false;
    for line in md.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            if inside {
                break;
            }
            inside = h.trim_start().starts_with(name);
            continue;
        }
        if inside {
            out.push_str(line.trim());
            out.push(' ');
        }
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Inline SVG of the intraday equity curve (rupees).
pub fn equity_svg(points: &[(Ts, i64)]) -> String {
    let (w, h, pad) = (720.0, 240.0, 40.0);
    if points.len() < 2 {
        return "<p class=\"muted\">No closed trades.</p>".into();
    }
    let t0 = points[0].0.timestamp() as f64;
    let t1 = points.last().map_or(t0 + 1.0, |p| p.0.timestamp() as f64).max(t0 + 1.0);
    let lo = points.iter().map(|p| p.1).min().unwrap_or(0).min(0) as f64;
    let hi = points.iter().map(|p| p.1).max().unwrap_or(0).max(0) as f64;
    let span = (hi - lo).max(100.0);
    let x = |t: f64| pad + (t - t0) / (t1 - t0) * (w - 2.0 * pad);
    let y = |v: f64| h - pad - (v - lo) / span * (h - 2.0 * pad);
    let mut path = String::new();
    for (i, (t, v)) in points.iter().enumerate() {
        let _ = write!(
            path,
            "{}{:.1},{:.1} ",
            if i == 0 { "M" } else { "L" },
            x(t.timestamp() as f64),
            y(*v as f64)
        );
    }
    let last = points.last().map_or(0, |p| p.1);
    let colour = if last >= 0 { "#1a7f37" } else { "#cf222e" };
    format!(
        r##"<svg viewBox="0 0 {w} {h}" role="img" aria-label="Intraday equity">
<line x1="{pad}" x2="{x2}" y1="{y0:.1}" y2="{y0:.1}" stroke="#8c959f" stroke-dasharray="4 4"/>
<path d="{path}" fill="none" stroke="{colour}" stroke-width="2"/>
<text x="{pad}" y="16" font-size="12">₹{hi_l}</text><text x="{pad}" y="{yb}" font-size="12">₹{lo_l}</text>
<text x="{x2}" y="{yl:.1}" font-size="12" text-anchor="end">₹{last_l}</text>
</svg>"##,
        x2 = w - pad,
        y0 = y(0.0),
        yb = h - 8.0,
        yl = y(last as f64) - 6.0,
        hi_l = Price(hi as i64),
        lo_l = Price(lo as i64),
        last_l = Price(last),
    )
}

pub fn eod_html(e: &Eod) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Saarthi EOD {d}</title>
<style>
:root{{color-scheme:light dark;--fg:#1f2328;--bg:#fff;--muted:#59636e;--line:#d1d9e0}}
@media (prefers-color-scheme:dark){{:root{{--fg:#e6edf3;--bg:#0d1117;--muted:#9198a1;--line:#3d444d}}}}
body{{font:14px/1.5 system-ui,sans-serif;color:var(--fg);background:var(--bg);max-width:980px;margin:0 auto;padding:16px}}
table{{border-collapse:collapse;width:100%;margin:8px 0 20px;font-variant-numeric:tabular-nums}}
th,td{{border-bottom:1px solid var(--line);padding:4px 8px;text-align:left}} td.n{{text-align:right}}
.muted{{color:var(--muted)}} svg{{width:100%;height:auto}} pre{{white-space:pre-wrap}}
.wrap{{overflow-x:auto}}
</style></head><body><h1>Saarthi EOD {d}</h1>"#,
        d = e.date
    );
    let _ = write!(s, "<pre>{}</pre>", esc(&eod_text(e)));
    let _ = write!(s, "<h2>Equity (net, ₹)</h2>{}", equity_svg(&equity_points(&e.trades)));
    s.push_str("<h2>Trades</h2><div class=\"wrap\"><table><tr><th>Symbol</th><th>Strategy</th><th>Side</th><th>Qty</th><th>Signal</th><th>Entry</th><th>Exit</th><th>Slip/sh</th><th>Charges</th><th>Net</th><th>In</th><th>Out</th><th>Exit reason</th></tr>");
    for t in &e.trades {
        let _ = write!(
            s,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(t.symbol.as_str()),
            esc(&t.strategy),
            if t.side.sign() > 0 { "long" } else { "short" },
            t.entry_qty,
            t.signal_px,
            opt(t.avg_entry()),
            opt(t.avg_exit()),
            opt(slippage(t)),
            t.charges,
            t.net(None),
            hm(t.opened_at),
            hm(t.closed_at),
            esc(t.exit_reason.as_deref().unwrap_or(t.state.as_str())),
        );
    }
    s.push_str("</table></div><h2>By strategy</h2><table><tr><th>Strategy</th><th>Trades</th><th>Wins</th><th>Net ₹</th><th>Charges ₹</th><th>Avg slippage ₹/sh</th></tr>");
    for (k, v) in per_strategy(&e.trades) {
        let _ = write!(
            s,
            "<tr><td>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td></tr>",
            esc(&k),
            v.trades,
            v.wins,
            v.net,
            v.charges,
            v.avg_slippage
        );
    }
    s.push_str("</table><h2>Incidents</h2>");
    let inc = incidents(e);
    if inc.is_empty() {
        s.push_str("<p class=\"muted\">None.</p>");
    } else {
        s.push_str("<table><tr><th>Time</th><th>Severity</th><th>Message</th></tr>");
        for (t, sev, m) in inc {
            let _ = write!(s, "<tr><td>{t}</td><td>{}</td><td>{}</td></tr>", esc(&sev), esc(&m));
        }
        s.push_str("</table>");
    }
    if let Some(d) = &e.summary
        && !d.rejected.is_empty()
    {
        s.push_str("<h2>Signals refused</h2><table><tr><th>Reason</th><th>Count</th></tr>");
        for (k, v) in &d.rejected {
            let _ = write!(s, "<tr><td>{}</td><td class=n>{v}</td></tr>", esc(k));
        }
        s.push_str("</table>");
    }
    if !e.recent.is_empty() {
        s.push_str("<h2>Recent sessions</h2><table><tr><th>Date</th><th>Trades</th><th>W/L</th><th>Net ₹</th><th>Flat</th><th>Restarts</th></tr>");
        for (d, x) in e.recent.iter().rev() {
            let _ = write!(
                s,
                "<tr><td>{d}</td><td class=n>{}</td><td class=n>{}/{}</td><td class=n>{}</td><td>{}</td><td class=n>{}</td></tr>",
                x.trades,
                x.wins,
                x.losses,
                x.net,
                if x.flat_after_square_off { "yes" } else { "NO" },
                x.restarts
            );
        }
        s.push_str("</table>");
    }
    s.push_str("<h2>Strategist journal</h2>");
    match &e.journal {
        Some(j) => {
            let _ = write!(s, "<pre>{}</pre>", esc(j));
        }
        None => s.push_str("<p class=\"muted\">No journal for today.</p>"),
    }
    s.push_str("<p class=\"muted\">Prices in rupees. Charges are the cost model's estimate; check them against the contract note.</p></body></html>\n");
    s
}

/// The hourly Telegram update from the control API's answers.
pub fn hourly_text(now: Ts, state: &Value, pnl: &Value, positions: &Value, note: Option<&str>) -> String {
    let rs = |v: &Value| v.as_i64().map(|p| Price(p).to_string()).unwrap_or_else(|| "—".into());
    let mut s = String::new();
    let _ = writeln!(s, "⏱ Saarthi {} IST", hm(Some(now)));
    let _ = writeln!(
        s,
        "Day P&L ₹{} | trades {} (W {} L {}) | loss budget used {:.0}%",
        rs(&pnl["day_pnl"]),
        pnl["trades"],
        pnl["wins"],
        pnl["losses"],
        pnl["loss_limit_used"].as_f64().unwrap_or(0.0) * 100.0
    );
    let _ = writeln!(
        s,
        "Engine {} | feed {} | halted: {} | plan v{}{}",
        state["phase"].as_str().unwrap_or("?"),
        if state["feed_ok"].as_bool() == Some(true) {
            "ok"
        } else {
            "DOWN"
        },
        state["halted"].as_str().unwrap_or("no"),
        state["plan_version"],
        if state["plan_is_default"].as_bool() == Some(true) {
            " (default)"
        } else {
            ""
        }
    );
    let ps = positions["positions"].as_array().cloned().unwrap_or_default();
    if ps.is_empty() {
        let _ = writeln!(s, "No open positions.");
    } else {
        for p in &ps {
            let _ = writeln!(
                s,
                "• {} {} {} @ ₹{} SL ₹{} → ₹{}",
                p["symbol"].as_str().unwrap_or("?"),
                p["side"].as_str().unwrap_or("?"),
                p["qty"],
                rs(&p["avg_entry"]),
                rs(&p["stop"]),
                rs(&p["net"])
            );
        }
    }
    if let Some(n) = note {
        let _ = writeln!(s, "\n🧭 {}", n.trim());
    }
    s
}

/// The newest entry of today's hourly journal (`<date>-hourly.md`), if it
/// was written in the last `max_age` minutes.
pub fn latest_note(journal_dir: &Path, date: NaiveDate) -> Option<String> {
    let s = std::fs::read_to_string(journal_dir.join(format!("{date}-hourly.md"))).ok()?;
    let last = s.rsplit("<!--").next()?;
    let body = last.split_once("-->").map(|(_, b)| b)?.trim();
    (!body.is_empty()).then(|| body.to_string())
}

/// PNG of the equity curve for Telegram. `None` when no font is available
/// (the text report is still sent).
pub fn equity_png(points: &[(Ts, i64)], title: &str) -> Option<Vec<u8>> {
    use plotters::prelude::*;

    if points.len() < 2 || !fonts::ready() {
        return None;
    }
    // plotters encodes PNG only to a file.
    let dir = std::env::temp_dir().join(format!("saarthi-report-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("equity.png");
    let drawn = (|| -> Option<()> {
        let root = BitMapBackend::new(&path, (900, 420)).into_drawing_area();
        root.fill(&WHITE).ok()?;
        let t0 = points[0].0;
        let mins = |t: Ts| (t - t0).num_seconds() as f64 / 60.0;
        let x_max = mins(points.last()?.0).max(1.0);
        let rupees: Vec<(f64, f64)> = points.iter().map(|(t, v)| (mins(*t), *v as f64 / 100.0)).collect();
        let lo = rupees.iter().map(|p| p.1).fold(0.0f64, f64::min);
        let hi = rupees.iter().map(|p| p.1).fold(0.0f64, f64::max);
        let pad = ((hi - lo) * 0.1).max(1.0);
        let mut chart = ChartBuilder::on(&root)
            .caption(title, ("sans-serif", 22))
            .margin(12)
            .x_label_area_size(36)
            .y_label_area_size(70)
            .build_cartesian_2d(0.0..x_max, (lo - pad)..(hi + pad))
            .ok()?;
        let start = t0.with_timezone(&saarthi_core::IST);
        chart
            .configure_mesh()
            .light_line_style(WHITE)
            .x_desc("IST")
            .y_desc("net ₹")
            .x_label_formatter(&|m| {
                (start + TimeDelta::seconds((*m * 60.0) as i64))
                    .format("%H:%M")
                    .to_string()
            })
            .draw()
            .ok()?;
        let colour = if rupees.last()?.1 >= 0.0 {
            RGBColor(26, 127, 55)
        } else {
            RGBColor(207, 34, 46)
        };
        chart
            .draw_series(LineSeries::new(rupees, colour.stroke_width(3)))
            .ok()?;
        chart
            .draw_series(LineSeries::new(vec![(0.0, 0.0), (x_max, 0.0)], BLACK.mix(0.3)))
            .ok()?;
        root.present().ok()
    })();
    let bytes = drawn.and_then(|_| std::fs::read(&path).ok());
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

/// Registers a system TrueType font for `sans-serif` once.
pub mod fonts {
    use std::sync::OnceLock;

    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ];

    static READY: OnceLock<bool> = OnceLock::new();

    pub fn ready() -> bool {
        *READY.get_or_init(|| {
            let from_env = std::env::var("SAARTHI_FONT").ok();
            for p in from_env.iter().map(String::as_str).chain(CANDIDATES.iter().copied()) {
                if let Ok(bytes) = std::fs::read(p) {
                    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                    if plotters::style::register_font("sans-serif", plotters::style::FontStyle::Normal, leaked).is_ok()
                    {
                        return true;
                    }
                }
            }
            tracing::warn!("no TrueType font found (install fonts-dejavu-core or set SAARTHI_FONT); charts skipped");
            false
        })
    }
}
