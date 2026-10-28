# Saarthi

Autonomous intraday equity trading on NSE through Angel One SmartAPI. A Rust
engine executes; Claude acts as an off-path strategist whose plans are clamped
by hard risk limits in code.

Specs live in [`docs/`](docs): [PLAN](docs/PLAN.md) (milestones),
[ARCHITECTURE](docs/ARCHITECTURE.md), [AGENT](docs/AGENT.md) (strategist) and
[BUILD](docs/BUILD.md).

> **Status: milestone M1 (market data + indicators).** The workspace can log
> in, download the instrument master and historical candles, stream live ticks
> over SmartAPI WebSocket 2.0, and build 1m/5m bars, session VWAP, the opening
> range and incremental indicators. It **cannot place orders**: every order
> method returns `Unsupported` until the M3 risk guard and OMS exist.

## Layout

| Path | What |
|---|---|
| `crates/core` (`saarthi-core`) | Money as integer paise, instruments, bars, timeframes, `MarketClock` (IST phases + NSE holidays) |
| `crates/broker` (`saarthi-broker`) | `Broker` trait; SmartAPI adapter: TOTP login, re-login on expiry, throttling, retries, instrument master, candles |
| `crates/store` (`saarthi-store`) | Atomic Parquet writers/readers for bars and instruments; resume manifest |
| `crates/feed` (`saarthi-feed`) | SmartAPI WebSocket 2.0: binary decode, subscribe, heartbeat, reconnect with backoff, staleness watchdog, raw-frame recorder |
| `crates/bars` (`saarthi-bars`) | Tick → 1m → 5m bars, session VWAP, opening range, per-symbol indicator `Snapshot` |
| `crates/ta` (`saarthi-ta`) | Incremental EMA, ATR, RSI, VWAP bands, volume z-score (checked against TA-Lib) |
| `bins/fetch` (`fetch`) | Market-data downloader CLI |
| `bins/feedcheck` (`feedcheck`) | Records a live session and checks its bars against Angel's candles (M1 exit) |
| `config/` | `engine.toml`, `risk.toml`, `universe.toml` (NIFTY 100), `holidays.toml`, `secrets.env.example` |

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
