# Saarthi

Autonomous intraday equity trading on NSE through Angel One SmartAPI. A Rust
engine executes; Claude acts as an off-path strategist whose plans are clamped
by hard risk limits in code.

Specs live in [`docs/`](docs): [PLAN](docs/PLAN.md) (milestones),
[ARCHITECTURE](docs/ARCHITECTURE.md), [AGENT](docs/AGENT.md) (strategist) and
[BUILD](docs/BUILD.md).

> **Status: milestone M0 (foundations).** The workspace can log in, download
> the instrument master and historical candles. It **cannot place orders**:
> every order method returns `Unsupported` until the M3 risk guard and OMS exist.

## Layout

| Path | What |
|---|---|
| `crates/core` (`saarthi-core`) | Money as integer paise, instruments, bars, timeframes, `MarketClock` (IST phases + NSE holidays) |
| `crates/broker` (`saarthi-broker`) | `Broker` trait; SmartAPI adapter: TOTP login, re-login on expiry, throttling, retries, instrument master, candles |
| `crates/store` (`saarthi-store`) | Atomic Parquet writers/readers for bars and instruments; resume manifest |
| `bins/fetch` (`fetch`) | Market-data downloader CLI |
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

Broker tests run against a mocked HTTP server (`wiremock`), so no Angel One
credentials are needed.

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
