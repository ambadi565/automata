# PLAN — Saarthi: Autonomous Intraday Trading on Angel One

## Goal
A fully unattended system that, on every NSE trading day:
- starts itself before the open, logs in, researches, and plans,
- trades intraday equity from 09:15 to 15:10 using backtested strategies,
- squares off everything before the close,
- sends a Telegram report every hour and a full report at end of day,
- needs **zero human input** (a Telegram `/halt` exists only as an emergency brake).

## Success criteria (realistic)
Turning X into kX in a day is not a design target. Chasing it forces oversized bets, and most retail intraday traders lose money. The system optimizes for:
- positive expectancy after **all charges** over 3+ months of paper and live data,
- max daily drawdown ≤ `max_daily_loss_pct` (default 2%), with **zero** limit breaches,
- Sharpe > 1.5 in walk-forward backtests before any live capital,
- 100% days ending flat, with 100% of reports delivered.

`daily_profit_lock_pct` lets you stop for the day after a target is hit. It stops trading at a target; it never pushes the system to reach one.

---

## Milestones

### M0 — Foundations (Week 1)
- [ ] Angel One SmartAPI app created; API key, client code, PIN, and TOTP secret stored
- [ ] Rust workspace skeleton (`core`, `broker`, `store`) + CI (fmt, clippy, tests)
- [ ] `fetch` binary: instrument master + historical candles → Parquet
- [ ] Telegram bot created, chat ID captured
- **Exit:** `cargo run -p fetch` downloads 1 year of 5-min data for NIFTY 100

### M1 — Market data + indicators (Week 2)
- [x] `feed`: SmartAPI WebSocket 2.0, binary decode, reconnect, staleness
- [x] `bars`: tick→1m→5m, session VWAP, opening range
- [x] `ta`: incremental EMA/ATR/RSI/VWAP bands/volume z-score, with tests vs reference values
- [ ] Exit check on a live session: `feedcheck record`, then `feedcheck compare` (needs credentials)
- **Exit:** live 1-min bars match Angel's candles for a full session (±1 tick)

### M2 — Strategies + backtester (Weeks 3–4)
- [x] `strategy` trait + ORB, VWAP reversion, momentum
- [x] `broker::Backtest` with a realistic cost model (brokerage, STT, exchange, SEBI, GST, stamp duty) and slippage
- [x] `backtest` binary: walk-forward, per-strategy metrics, equity curve, gate check
- [ ] Run the gate on 1+ year of real NIFTY 100 5-min candles (needs `fetch` with credentials; only synthetic data so far)
- **Gate:** a strategy goes live only if out-of-sample shows PF > 1.3, max DD < 8%, ≥ 200 trades

### M3 — Risk + OMS + paper trading (Weeks 5–6)
- [x] `risk` with every check in ARCHITECTURE §5.7, each unit-tested with breach scenarios
- [x] `oms` state machine, SL pairing, reconciliation, square-off
- [x] `broker::Paper` driven by the live feed
- [x] `store`: SQLite WAL schema + crash-recovery test (kill -9 mid-trade → restart → state intact)
- [x] `engine` daemon (paper mode), offline `replay`, kill switch, `exit-check`
- [ ] Run 5 paper sessions (needs credentials and market days)
- **Exit:** 5 consecutive paper days with no crash and all positions flat by 15:10

### M4 — Claude strategist (Week 7)
- [ ] Control API (`axum`), plan schema, validation, and clamping
- [ ] Python agent (Claude Agent SDK) with tools from AGENT.md
- [ ] Pre-market, intraday-review, and EOD prompts; JSON-schema-validated outputs
- [ ] Fallback `plan.default.json`
- **Exit:** the strategist produces valid plans 20/20 runs; a garbage plan is clamped or rejected in tests

### M5 — Reporting + autonomy (Week 8)
- [ ] Hourly + EOD reports (Telegram text + PNG + HTML archive)
- [ ] systemd services and timers, holiday calendar, boot checks
- [ ] VPS provisioning (Mumbai, static IP), chrony, log rotation, backups
- **Exit:** 10 consecutive fully unattended paper days with every report delivered

### M6 — Live, small (Weeks 9–12)
- [ ] Static IP registered with Angel One; compliance checklist done
- [ ] Live with **minimal capital** you can afford to lose; risk limits halved
- [ ] Weekly review: live vs paper vs backtest slippage
- **Exit:** 20 live days, zero limit breaches, live results within the backtest confidence band

### M7 — Iterate
- Add strategies only through the M2 gate. Consider F&O only after 3 profitable live months.

---

## Risk register

| Risk | Mitigation |
|---|---|
| Strategy has no edge | Backtest gate, paper period, small live size |
| Overfitting | Walk-forward, out-of-sample, parameter stability checks |
| LLM hallucinated plan | Schema validation + clamping; LLM can't place orders |
| Connectivity/hardware failure | VPS, broker-side SL, systemd restart, reconciliation |
| Broker API changes or outages | Adapter isolated in `broker`; alerting on error rates |
| Regulatory non-compliance | Static IP, order tagging, rate limits; verify SEBI/Angel rules |
| Runaway losses | Daily loss stop, consecutive-loss cooldown, kill switch |

## Budget (monthly, approx.)
- VPS (2 vCPU, 4 GB, Mumbai): ₹1,000–2,000
- Claude API: depends on run frequency. ~30 strategist calls/day is modest; cap with a daily token budget in config
- Angel One SmartAPI: free (check current terms)
