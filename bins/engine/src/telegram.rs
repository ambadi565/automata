//! Telegram commands (ARCHITECTURE §5.10): `/status`, `/pnl`, `/positions`,
//! `/plan`, `/halt [reason]`, `/resume`. Only the configured chat is obeyed.
//! Commands go through the same request channel as the control API, so the
//! engine loop answers them between ticks.

use std::time::Duration;

use saarthi_control::{Handle, Reply, Request};
use saarthi_notify::{Command, Telegram};
use serde_json::Value;

/// Commands older than this when the engine first sees them are not obeyed:
/// a `/resume` typed yesterday must not fire this morning.
pub const STALE_SECS: i64 = 120;

const HELP: &str = "Saarthi commands:\n/status: engine phase, feed, halts, plan\n/pnl: today's P&L\n\
/positions: open positions\n/plan: the plan in force\n/halt [reason]: flatten everything and stop new entries\n\
/resume: lift a manual halt (refused after 14:45 or after the daily loss stop)";

fn rs(v: &Value) -> String {
    match v.as_i64() {
        Some(p) => {
            let sign = if p < 0 { "-" } else { "" };
            let a = p.unsigned_abs();
            format!("{sign}₹{}.{:02}", a / 100, a % 100)
        }
        None => "—".into(),
    }
}

/// What to send back for `cmd` given the engine's answer.
pub fn render(cmd: &str, r: &Reply) -> String {
    let v = match r {
        Ok(v) => v,
        Err((code, msg)) => return format!("✖ {cmd}: {msg} ({code})"),
    };
    match cmd {
        "status" => format!(
            "Phase {} | feed {} | entries {}\nHalted: {}\nPlan v{}{} until {}\nCapital {} | restarts today {}",
            v["phase"].as_str().unwrap_or("?"),
            if v["feed_ok"].as_bool() == Some(true) {
                "ok"
            } else {
                "DOWN"
            },
            if v["entries_open"].as_bool() == Some(true) {
                "open"
            } else {
                "closed"
            },
            v["halted"].as_str().unwrap_or("no"),
            v["plan_version"],
            if v["plan_is_default"].as_bool() == Some(true) {
                " (default)"
            } else {
                ""
            },
            v["plan_valid_until"].as_str().unwrap_or("?"),
            rs(&v["capital"]),
            v["restarts_today"],
        ),
        "pnl" => format!(
            "Day P&L {} (net closed {}, charges {})\nTrades {} | W {} L {} | win rate {:.0}%\n\
             Worst {} | loss limit used {:.0}% of {}",
            rs(&v["day_pnl"]),
            rs(&v["net"]),
            rs(&v["charges"]),
            v["trades"],
            v["wins"],
            v["losses"],
            v["win_rate"].as_f64().unwrap_or(0.0) * 100.0,
            rs(&v["worst_pnl"]),
            v["loss_limit_used"].as_f64().unwrap_or(0.0) * 100.0,
            rs(&v["daily_loss_limit"]),
        ),
        "positions" => {
            let ps = v["positions"].as_array().cloned().unwrap_or_default();
            if ps.is_empty() {
                return "No open positions.".into();
            }
            ps.iter()
                .map(|p| {
                    format!(
                        "{} {} {} @ {} | SL {} | LTP {} | {}",
                        p["symbol"].as_str().unwrap_or("?"),
                        p["side"].as_str().unwrap_or("?"),
                        p["qty"],
                        rs(&p["avg_entry"]),
                        rs(&p["stop"]),
                        rs(&p["ltp"]),
                        rs(&p["net"]),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "plan" => {
            let p = &v["plan"];
            let syms: Vec<&str> = p["watchlist"]
                .as_array()
                .map(|w| w.iter().filter_map(|i| i["symbol"].as_str()).collect())
                .unwrap_or_default();
            format!(
                "Plan v{}{}: bias {}, regime {}, risk ×{}\nWatchlist: {}\nPaused: {}\n{}",
                p["version"],
                if v["is_default"].as_bool() == Some(true) {
                    " (default)"
                } else {
                    ""
                },
                p["market_bias"].as_str().unwrap_or("?"),
                p["regime"].as_str().unwrap_or("?"),
                p["global_risk_multiplier"],
                if syms.is_empty() {
                    "all symbols".into()
                } else {
                    syms.join(", ")
                },
                p["paused_strategies"],
                p["rationale"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(600)
                    .collect::<String>(),
            )
        }
        "halt" => "🛑 Halted: positions are being flattened and no new entries will be taken.".into(),
        "resume" => "▶️ Resumed: new entries allowed again.".into(),
        _ => HELP.into(),
    }
}

pub fn request_for(c: &Command) -> Option<Request> {
    Some(match c.name.as_str() {
        "status" => Request::State,
        "pnl" => Request::Pnl,
        "positions" => Request::Positions,
        "plan" => Request::Plan,
        "halt" | "stop" => Request::Halt(if c.args.is_empty() {
            "telegram /halt".into()
        } else {
            format!("telegram: {}", c.args.chars().take(200).collect::<String>())
        }),
        "resume" => Request::Resume,
        _ => return None,
    })
}

/// Polls Telegram and answers commands until the runtime stops.
pub async fn serve(tg: Telegram, handle: Handle, started_unix: i64) {
    let mut offset = 0i64;
    let mut backoff = 1u64;
    loop {
        match tg.commands(offset, 25).await {
            Ok((cmds, next)) => {
                backoff = 1;
                offset = next;
                for c in cmds {
                    let name = if c.name == "stop" {
                        "halt".to_string()
                    } else {
                        c.name.clone()
                    };
                    let text = if c.date < started_unix - STALE_SECS {
                        format!("Ignored /{} sent before the engine started; send it again.", c.name)
                    } else if let Some(req) = request_for(&c) {
                        tracing::warn!(command = %c.name, "telegram command");
                        render(&name, &handle.ask(req).await)
                    } else {
                        HELP.to_string()
                    };
                    if let Err(e) = tg.send_text(&text).await {
                        tracing::warn!(error = %e, "telegram reply failed");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "telegram polling failed");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd(name: &str, args: &str) -> Command {
        Command {
            update_id: 1,
            date: 0,
            name: name.into(),
            args: args.into(),
        }
    }

    #[test]
    fn commands_map_to_requests() {
        assert_eq!(
            request_for(&cmd("halt", "")),
            Some(Request::Halt("telegram /halt".into()))
        );
        assert_eq!(
            request_for(&cmd("halt", "news")),
            Some(Request::Halt("telegram: news".into()))
        );
        assert_eq!(request_for(&cmd("resume", "")), Some(Request::Resume));
        assert_eq!(request_for(&cmd("buy", "RELIANCE")), None);
    }

    #[test]
    fn renders_rupees_and_errors() {
        let pnl = json!({"day_pnl": -123456, "net": -100000, "charges": 2345, "trades": 3, "wins": 1,
            "losses": 2, "win_rate": 0.333, "worst_pnl": -150000, "loss_limit_used": 0.08,
            "daily_loss_limit": 2000000});
        let t = render("pnl", &Ok(pnl));
        assert!(t.starts_with("Day P&L -₹1234.56"), "{t}");
        assert!(t.contains("8% of ₹20000.00"), "{t}");
        assert_eq!(
            render("resume", &Err((409, "after 14:45".into()))),
            "✖ resume: after 14:45 (409)"
        );
        assert_eq!(render("positions", &Ok(json!({"positions": []}))), "No open positions.");
    }
}
