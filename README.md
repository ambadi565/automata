# Saarthi

Autonomous intraday equity trading on NSE through Angel One SmartAPI. A Rust
engine executes; Claude acts as an off-path strategist whose plans are clamped
by hard risk limits in code.

Specs live in [`docs/`](docs): [PLAN](docs/PLAN.md) (milestones),
[ARCHITECTURE](docs/ARCHITECTURE.md), [AGENT](docs/AGENT.md) (strategist) and
[BUILD](docs/BUILD.md).

> **Status: milestone M2 (strategies + backtester).** The workspace can log
> in, download the instrument master and historical candles, stream live ticks
> over SmartAPI WebSocket 2.0, build 1m/5m bars, session VWAP, the opening
> range and incremental indicators, and backtest three strategies walk-forward
> against Indian intraday charges and slippage. It **cannot place real
> orders**: every SmartAPI order method returns `Unsupported` until the M3 risk
> guard and OMS exist, and the backtest broker has no network code.

## Layout

| Path | What |
|---|---|
| `crates/core` (`saarthi-core`) | Money as integer paise, instruments, bars, timeframes, `MarketClock` (IST phases + NSE holidays) |
| `crates/broker` (`saarthi-broker`) | `Broker` trait; SmartAPI adapter: TOTP login, re-login on expiry, throttling, retries, instrument master, candles |
| `crates/store` (`saarthi-store`) | Atomic Parquet writers/readers for bars and instruments; resume manifest |
| `crates/feed` (`saarthi-feed`) | SmartAPI WebSocket 2.0: binary decode, subscribe, heartbeat, reconnect with backoff, staleness watchdog, raw-frame recorder |
| `crates/bars` (`saarthi-bars`) | Tick → 1m → 5m bars, session VWAP, opening range, per-symbol indicator `Snapshot` |
| `crates/ta` (`saarthi-ta`) | Incremental EMA, ATR, RSI, VWAP bands, volume z-score (checked against TA-Lib) |
| `crates/strategy` (`saarthi-strategy`) | `Strategy` trait; ORB, VWAP reversion, trend momentum; parameter specs and optimiser grids |
| `crates/broker::backtest` | `Backtest` broker: simulated exchange with Indian intraday charges (`config/costs.toml`) and slippage |
| `bins/backtest` (`backtest`) | Walk-forward backtester, per-strategy metrics, equity curve, M2 go-live gate |
| `bins/fetch` (`fetch`) | Market-data downloader CLI |
| `bins/feedcheck` (`feedcheck`) | Records a live session and checks its bars against Angel's candles (M1 exit) |
| `config/` | `engine.toml`, `risk.toml`, `universe.toml` (NIFTY 100), `holidays.toml`, `costs.toml`, `backtest.toml`, `secrets.env.example` |

## Quick start

```bash
cp config/secrets.env.example config/secrets.env && chmod 600 config/secrets.env
# fill in ANGEL_API_KEY, ANGEL_CLIENT_CODE, ANGEL_MPIN, ANGEL_TOTP_SECRET

cargo run -p fetch -- login-check          # verifies credentials; read-only
cargo run --release -p fetch               # instruments + 1 year of 5-min candles for NIFTY 100
cargo run --release -p fetch -- candles --tf 1d --days 730
```

Output: `data/instruments.parquet` and `data/candles/<tf>/<SYMBOL>.parquet`
(prices as `Int64` paise, `ts` as UTC ms tagged `Asia/Kolkata`). The fetcher
spaces historical requests at 2/s (SmartAPI documents 3/s), retries rate-limit
and server errors with backoff, and writes after every request. Interrupt it
any time and rerun: it resumes from `data/candles/<tf>/_manifest.json` and only
refetches the last (possibly partial) day.

Refresh the universe after NSE's semi-annual rebalance:
download `ind_nifty100list.csv` from niftyindices.com, then
`cargo run -p fetch -- universe --csv ind_nifty100list.csv`.

Refresh `config/holidays.toml` every December. The clock refuses to answer for
a year the file does not cover, so a stale file means "don't trade", never
"trade on a holiday".

## Live feed check (M1 exit)

The M1 exit criterion is "live 1-min bars match Angel's candles for a full
session (±1 tick)". On a trading day:

```bash
cargo run --release -p feedcheck -- record            # start before 09:15; stops at 15:31
# after the close:
cargo run --release -p fetch -- candles --tf 1m --from 2026-10-28 --to 2026-10-28
cargo run --release -p feedcheck -- compare --date 2026-10-28
```

`record` subscribes the universe in Quote mode, writes every raw frame to
`data/live/<date>/frames.bin` and the bars it builds to
`data/live/<date>/1m/`. `compare` prints PASS/FAIL per symbol with the
minutes that differ. Bars after a feed drop are marked partial and skipped.
If the 09:15 bar is the only mismatch, flip `[bars] pre_open` in
`engine.toml` and run `feedcheck replay --date …` to rebuild the bars from the
recording without waiting for another session.

## Backtest (M2)

```bash
cargo run --release -p fetch -- candles --tf 5m --days 365     # real data first
cargo run --release -p backtest -- --strategy all --walk-forward 60:20 --out data/reports/bt
```

Each strategy is optimised on 60 trading days over its parameter grid,
traded on the next 20 with the winner, and rolled forward; only those test
segments count. Per strategy, `data/reports/bt/<strategy>/` gets
`report.md`, `summary.json` (metrics + gate), `trades.csv`, `equity.csv`,
`equity.svg` and `windows.csv`; `overview.md` compares them. Add
`--fail-on-gate` to exit with status 2 unless every strategy passes.

The **gate** (PLAN M2), judged on out-of-sample results only: profit factor
\> 1.3, max drawdown < 8 %, ≥ 200 trades, and parameter stability (≥ 60 % of
the chosen parameters' grid neighbours also profitable in-sample).
Thresholds live in `config/backtest.toml`.

How a backtest trades:

- Signals form at a bar's close and fill at the **next** bar's open; a test
  proves past trades don't change when future bars do.
- Entries are bracket orders: market entry, SL-M stop, limit target. If a bar
  could have hit both, the stop wins. Gaps through a stop fill at the open.
  Targets need price to trade one tick through.
- Sizing and limits come from `risk.toml`: risk per trade, max positions,
  gross exposure, daily loss stop, trades per day, consecutive-loss
  cooldown, no new entries after 14:45, square-off at 15:10.
- Charges per executed order from `config/costs.toml` (Angel One brokerage,
  STT, NSE transaction charge, SEBI fee, IPFT, stamp duty, GST), plus 2 bps
  + 1 tick slippage on market and stop fills. Rates change; check
  `as_of` against a recent contract note.

No market data? `backtest synth` writes synthetic candles to exercise the
pipeline. Every report built on them is stamped **SYNTHETIC DATA**:
`--mode regimes` plants trend and range days for the strategies to find
(passing the gate there proves nothing), `--mode random-walk` has no
structure, and nothing should pass on it after costs.

```bash
cargo run --release -p backtest -- synth --data-dir /tmp/syn --symbols 30 --days 250
cargo run --release -p backtest -- --data-dir /tmp/syn --universe /tmp/syn/synthetic_universe.toml --out /tmp/syn/reports
```

## Multiple accounts

Nothing is tied to a single account. Credentials are a value passed to the
broker, not a global. Each account gets its own secrets file (and, if needed,
its own config dir):

```bash
fetch --secrets config/profiles/alice/secrets.env --config config/profiles/alice/engine.toml login-check
SAARTHI_SECRETS=/etc/saarthi/bob.env fetch candles
```

Market data under `data/` is shareable across accounts on one host;
per-account state will live under `data/accounts/<client_code>/`.

## Test

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace        # or: cargo nextest run --workspace
```

Broker tests run against a mocked HTTP server (`wiremock`) and feed tests
against a local WebSocket server sending synthetic binary frames, so no Angel
One credentials are needed. Indicator reference values come from TA-Lib; see
`crates/ta/tests/data/gen_reference.py` to regenerate them.

## Selling this to other people: regulatory note

Running this on your own account is personal algo trading under SEBI's retail
algo framework (static IP registration, order tagging, order-rate thresholds).
**Offering it to other people is different.** SEBI's framework (circular of
4 Feb 2025, phased in through 2025–26) requires algos offered to retail
investors to be provided through a broker, registered with the exchange, and
the algo provider to be empanelled with the exchange through that broker.
Selling signals or strategy recommendations can additionally require
Research Analyst (or Investment Adviser) registration. Confirm the current
rules with Angel One and a SEBI-registered compliance professional before
charging anyone or onboarding a single external user.
