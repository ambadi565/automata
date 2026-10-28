# ARCHITECTURE — Autonomous Intraday Trading System (Angel One)

> Codename: **Saarthi**. A Rust execution engine trades at machine speed.
> Claude acts as an off-path strategist that steers it. Hard risk limits in code
> bound everything Claude decides.

---

## 1. Design principles

1. **LLM is never on the order path.** Ticks go to signal, then risk, then order, in pure Rust in the microsecond-to-millisecond range. Claude's latency (seconds) affects only *what the plan is*, never *when an order fires*.
2. **The single variable latency is the LLM.** Everything else is deterministic, async, and in-process. There are no extra network hops besides the broker.
3. **Risk guard is sovereign.** Claude writes `plan.json` and the engine **clamps** it against `risk.toml`. A bad or hallucinated plan cannot exceed the limits.
4. **Fail flat.** Any unrecoverable state (lost feed, auth failure, reconciliation mismatch, daily loss hit) leads to cancelling all open orders, squaring off, and halting.
5. **Broker-side protection.** Every entry is paired with a broker-side stop-loss order, so positions stay protected if the machine or internet dies.
6. **Same code for backtest, paper and live.** Strategies run against a `Broker` trait with three implementations: `Backtest`, `Paper`, and `SmartApi`.

---

## 2. Tech stack

| Concern | Choice | Why |
|---|---|---|
| Engine, OMS, risk, feed | **Rust** (tokio) | Predictable latency, no GC pauses, strong typing for money and order state |
| WebSocket | `tokio-tungstenite` | SmartAPI WS 2.0 binary feed |
| REST | `reqwest` (rustls) | SmartAPI order and historical endpoints |
| TOTP login | `totp-rs` | Fully unattended daily login |
| Serialization | `serde`, `serde_json`, `toml` | Plan, config, and state |
| Indicators | in-house `ta` module (incremental) | O(1) updates per tick; no recomputation |
| Hot store | **SQLite (WAL)** via `rusqlite` (bundled) | Orders, fills, positions, events (crash recovery); synchronous, one writer, no runtime or build-time DB needed |
| Cold store | **Parquet** via `polars`/`arrow` | Ticks, 1-min bars, history for backtests |
| Local control API | `axum` on `127.0.0.1` | Claude's tools and the Telegram bot talk to the engine through it |
| Logging and metrics | `tracing` + JSON logs | Every decision is auditable |
| Strategist | **Anthropic Python SDK** (own tool loop), Claude Opus / Sonnet | Tool use, headless, scheduled; no shell or file tools |
| News and context | RSS (NSE announcements, Moneycontrol, ET Markets) + web search tool | Pre-market and intraday context |
| Reports | Telegram Bot API + PNG charts (`plotters`) + HTML/PDF EOD | Phone-first reporting |
| Scheduling | **systemd timers** (Linux VPS) | Survives reboots; no human start |
| Host | Ubuntu VPS in Mumbai (e.g. AWS `ap-south-1`) + Elastic/static IP | Low latency to NSE/Angel; static IP required for API order placement |

> A laptop works for development and paper trading. For live trading, use the VPS: a sleeping laptop or a change of Wi-Fi network is a real risk, and SEBI's retail-algo framework expects a registered static IP.

---

## 3. System diagram

```
                ┌──────────────────────── SLOW PATH (seconds–minutes) ────────────────────────┐
                │                                                                              │
  News/RSS ───► │  STRATEGIST (Anthropic SDK, Python)                                          │
  Web search ─► │   • pre-market: universe scan → watchlist + plan.json                        │
                │   • every 15 min: regime review → plan patch (pause/resize/retune)           │
                │   • hourly: narrative for report   • EOD: journal + next-day notes           │
                │        │ tools (HTTP, localhost)                         ▲                    │
                └────────┼─────────────────────────────────────────────────┼────────────────────┘
                         ▼                                                 │
                ┌──────────────── CONTROL API (axum, 127.0.0.1:7070) ──────┴──────┐
                │ GET /state /positions /pnl /bars /signals   POST /plan /halt    │
                └────────┬────────────────────────────────────────────────────────┘
                         ▼   validated + clamped plan (hot reload via watch channel)
┌─────────────────────────────── FAST PATH (µs–ms, single Rust process) ───────────────────────────────┐
│                                                                                                       │
│  SmartAPI WS ─► FEED ─► BAR BUILDER ─► INDICATORS ─► STRATEGY RUNNER ─► RISK GUARD ─► OMS ─► SmartAPI REST
│     ticks       │        1m/5m bars     VWAP,EMA,     ORB / VWAP-rev /   pre-trade     place/modify/cancel
│                 │                       ATR,RSI,OR    momentum signals   checks        + broker-side SL
│                 ▼                                                                       │
│              STORE (SQLite WAL + Parquet) ◄──────────────── fills / order updates ◄─────┘
│                 │                                                                       │
│                 └──► REPORTER (hourly + EOD) ──► Telegram                                │
│                                                                                                       │
│  SUPERVISOR: market clock • heartbeat • feed-staleness watchdog • reconciliation • square-off timer   │
└───────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 4. Rust workspace layout

```
saarthi/
├── Cargo.toml                 # workspace
├── config/
│   ├── engine.toml            # mode (backtest|paper|live), paths, schedule
│   ├── risk.toml              # HARD limits (Claude cannot edit)
│   ├── universe.toml          # allowed symbols + tokens (e.g. NIFTY 100)
│   └── secrets.env            # API key, client code, PIN, TOTP secret (chmod 600)
├── crates/
│   ├── core/        # types: Symbol, Price(i64 paise), Qty, Side, Order, Fill, Bar; MarketClock; errors
│   ├── broker/      # trait Broker; smartapi/{auth,rest,ws,instruments}; paper.rs; backtest.rs
│   ├── feed/        # WS client, reconnect/backoff, tick decode, staleness detection
│   ├── bars/        # tick→1m→5m aggregation, session VWAP, opening range
│   ├── ta/          # incremental indicators (EMA, ATR, RSI, VWAP bands, volume z-score)
│   ├── strategy/    # trait Strategy; orb.rs; vwap_revert.rs; momentum.rs; registry
│   ├── plan/        # plan.json schema, validation, clamping against risk.toml
│   ├── risk/        # pre-trade checks, exposure, daily P&L stop, kill switch
│   ├── oms/         # order state machine, SL pairing, retries, reconciliation
│   ├── store/       # sqlite (orders, fills, events, snapshots), parquet writers
│   ├── report/      # hourly/EOD metrics, charts (plotters), telegram sender
│   └── control/     # axum API for strategist + telegram commands
├── bins/
│   ├── engine/      # the live/paper daemon
│   ├── backtest/    # replay parquet through the same strategies
│   └── fetch/       # instrument master + historical candles downloader
├── strategist/      # Python: Claude strategist agent (see AGENT.md)
│   ├── agent.py
│   ├── tools/       # engine_api.py, news.py, history.py
│   └── prompts/     # premarket.md, intraday.md, eod.md
├── deploy/
│   ├── systemd/     # saarthi-engine.service, *.timer files
│   └── provision.sh
└── data/            # parquet/, saarthi.db, logs/, reports/
```

---

## 5. Module contracts

### 5.1 `core`
- Money is **integer paise (`i64`)**, never `f64`. Quantity is `u32`.
- `MarketClock` knows the IST session, NSE holidays (loaded yearly from `config/holidays.toml`), and the phases `PreOpen | Open | NoNewEntries | SquareOff | Closed`.

### 5.2 `broker`
```rust
#[async_trait]
pub trait Broker: Send + Sync {
    async fn login(&self) -> Result<Session>;
    async fn place(&self, req: OrderRequest) -> Result<BrokerOrderId>;
    async fn modify(&self, id: &BrokerOrderId, m: Modify) -> Result<()>;
    async fn cancel(&self, id: &BrokerOrderId) -> Result<()>;
    async fn positions(&self) -> Result<Vec<Position>>;
    async fn order_book(&self) -> Result<Vec<BrokerOrder>>;
    async fn funds(&self) -> Result<Funds>;
    async fn candles(&self, sym: &Symbol, tf: Tf, from: Ts, to: Ts) -> Result<Vec<Bar>>;
}
```
- `SmartApi`: TOTP login at boot, JWT refresh, rate limiter (token bucket per endpoint), order tagging, idempotency via a client order id → broker id map.
- `Paper`: fills at the next tick with configurable slippage and charges (brokerage, STT, exchange fees, GST, stamp duty).
- `Backtest`: deterministic replay clock.

### 5.3 `feed`
- Subscribes to tokens in the watchlist (LTP + quote + depth as needed).
- Exponential backoff reconnect. If there are **no ticks for N seconds in market hours**, it emits a `FeedStale` event and risk blocks new entries.

### 5.4 `bars` + `ta`
- Every indicator is **incremental** (`update(tick)` → O(1)).
- Exposes `Snapshot { bar_1m, bar_5m, vwap, ema9, ema21, atr14, rsi14, or_high, or_low, vol_z }` per symbol.

### 5.5 `strategy`
```rust
pub trait Strategy: Send {
    fn id(&self) -> &'static str;
    fn on_bar(&mut self, s: &Snapshot, p: &StrategyParams) -> Option<Signal>;
    fn on_tick(&mut self, t: &Tick, p: &StrategyParams) -> Option<Signal> { None }
}
pub struct Signal { sym, side, entry: Price, stop: Price, target: Option<Price>, confidence: f32, reason: String }
```
Initial strategies (each must pass the backtest gate before live):
- **ORB**: opening-range breakout (9:15–9:30 range), volume confirmation, ATR stop.
- **VWAP reversion**: fade 2σ VWAP-band extensions in range-bound regimes.
- **Trend momentum**: EMA9/21 alignment plus a pullback entry in trending regimes.

### 5.6 `plan`
Claude writes this; the engine validates and clamps it.
```json
{
  "version": 7,
  "valid_until": "2026-10-28T15:10:00+05:30",
  "market_bias": "bullish|bearish|neutral|avoid",
  "regime": "trend|range|volatile",
  "global_risk_multiplier": 0.8,
  "watchlist": [
    {
      "symbol": "RELIANCE-EQ",
      "strategies": ["orb", "momentum"],
      "direction": "long_only|short_only|both",
      "max_risk_pct": 0.5,
      "key_levels": {"support": [2810.5], "resistance": [2865.0]},
      "notes": "Results beat; gap up expected"
    }
  ],
  "paused_strategies": [],
  "rationale": "..."
}
```
Clamping rules: unknown symbols are dropped; `max_risk_pct` is capped at the risk limit; the multiplier is limited to [0, 1]; an expired plan falls back to `plan.default.json` (conservative) or halts.

### 5.7 `risk` (hard limits, `risk.toml`)
```toml
capital_base_inr          = 0            # read from funds at 09:00 if 0
max_risk_per_trade_pct    = 0.5          # of capital, distance entry→stop × qty
max_open_positions        = 3
max_gross_exposure_x      = 3.0          # incl. intraday leverage
max_daily_loss_pct        = 2.0          # → flatten + halt for the day
daily_profit_lock_pct     = 0          # 0 = off; if >0, stop new entries after hitting it
max_trades_per_day        = 15
max_consecutive_losses    = 4            # → cooldown 30 min
min_price_inr             = 50
no_new_entries_after      = "14:45"
square_off_at             = "15:10"
max_order_qty             = 2000
max_orders_per_second     = 5            # stay under SEBI/broker algo thresholds
```
Pre-trade checks, in order: kill switch, then phase, then feed health, then daily P&L, then trade count, then position count, then exposure, then per-trade risk sizing, then a price-band/circuit check, then a duplicate-signal check.

### 5.8 `oms`
Order state machine: `New → Sent → Open → PartiallyFilled → Filled | Rejected | Cancelled`.
- On entry fill, it **immediately places a broker-side SL-M**, plus a target or trailing stop managed by the engine.
- On reconcile (every 30 s and on any reconnect), broker positions and orders are compared with local state. If they mismatch, the engine trusts the broker and raises an alert. A severe mismatch triggers a halt.
- At 15:10 it cancels all orders and sends market exits for open positions, then verifies all positions are flat. It retries until flat and pages via Telegram if it can't.

### 5.9 `report`
- **Hourly** (10:15, 11:15, 12:15, 13:15, 14:15): realized/unrealized P&L, trades, win rate, open positions with SL, drawdown, risk used, feed and engine health, and Claude's 2–3 line commentary.
- **EOD** (~15:40): equity curve PNG, trade table, per-strategy stats, charges, slippage vs signal price, rule-violation log (should be empty), Claude's journal, and next-day watch notes. Sent to Telegram and saved as `reports/YYYY-MM-DD.html`.

### 5.10 `control`
Localhost-only HTTP API with a bearer token. Telegram commands: `/status`, `/pnl`, `/halt`, `/resume` (resume works only before 14:45 and only if the daily loss limit is not hit). This is a kill switch, not a requirement: the system runs with zero human input.

---

## 6. Daily timeline (IST, fully automatic)

| Time | Actor | Action |
|---|---|---|
| 08:30 | systemd | Boot check: NSE holiday? → exit. Disk, clock (chrony), network check |
| 08:35 | engine | TOTP login, download instrument master, fetch funds and set capital base |
| 08:40 | fetch | Pull last 60 days of 5-min + daily candles for the universe |
| 08:45 | strategist | **Pre-market run**: global cues, news, gaps, sector moves; writes `plan.json` (deadline 09:10) |
| 09:00 | engine | Load and clamp plan, subscribe WS, pre-open data |
| 09:15 | engine | Market open; build opening range |
| 09:30 → 14:45 | engine | Trading; strategies fire on bars and ticks |
| every 15 min | strategist | Intraday review → plan patch (optional) |
| hourly :15 | report | Hourly Telegram report |
| 14:45 | risk | No new entries |
| 15:10 | oms | Square-off all, verify flat |
| 15:30 | engine | Market close; flush stores, unsubscribe |
| 15:35 | strategist | EOD journal |
| 15:40 | report | EOD report → Telegram |
| 15:45 | systemd | Engine stops; backups to object storage |

---

## 7. Failure handling

| Failure | Detection | Response |
|---|---|---|
| Internet drop | WS closed / REST timeouts | Broker-side SL already protects positions; reconnect with backoff; block entries until healthy |
| Feed stale | No ticks > 10 s | Block entries; if > 120 s with open positions, flatten via REST |
| Auth expiry | 401/AG8001-type errors | Re-login via TOTP; if that fails, halt + alert |
| Engine crash | systemd `Restart=always` | On restart: rebuild state from broker + SQLite, re-arm SLs |
| Claude/API down | Strategist timeout | Engine continues on last valid plan; if none, `plan.default.json` |
| Order rejected | OMS status | Log reason; no blind retries for margin/price-band rejects |
| Daily loss hit | Risk P&L monitor | Flatten, halt, alert, EOD report still sent |
| Clock drift | chrony check | Refuse to start if drift > 500 ms |

---

## 8. Latency budget (target)

| Hop | Target |
|---|---|
| WS frame → decoded tick | < 50 µs |
| Tick → indicators → signal | < 200 µs |
| Signal → risk → order request built | < 100 µs |
| HTTP to broker (VPS Mumbai) | 5–30 ms (network-bound, unavoidable) |
| **Claude plan update** | 5–60 s (off path, the only "slow" component) |

---

## 9. Security

- Secrets live in `secrets.env` (chmod 600) or a systemd credential store, never in git or prompts.
- Claude's tools **cannot** place orders, read secrets, or edit `risk.toml`. It only writes plans through `/plan`.
- The control API binds to `127.0.0.1` only. Telegram commands are restricted to your chat ID.

---

## 10. Compliance note

SEBI's retail algorithmic-trading framework and Angel One's SmartAPI terms set rules on static IP registration, order tagging, order-rate thresholds, and algo registration. **Verify the current requirements with Angel One before going live**, and keep `max_orders_per_second` below the threshold that triggers mandatory algo registration unless you register.
