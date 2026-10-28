# BUILD — Setup, Build, Test, Deploy

## 1. Prerequisites
- Rust stable (`rustup`), `cargo-nextest`
- Python 3.11+, `uv`
- Angel One account with SmartAPI enabled: API key, client code, MPIN, TOTP secret (from the "Enable TOTP" QR)
- Telegram bot token (@BotFather) + your chat ID
- Anthropic API key
- For live trading: Ubuntu 24.04 VPS in Mumbai with a static IP

## 2. Secrets (`config/secrets.env`, chmod 600, gitignored)
```
ANGEL_API_KEY=
ANGEL_CLIENT_CODE=
ANGEL_MPIN=
ANGEL_TOTP_SECRET=
TELEGRAM_BOT_TOKEN=
TELEGRAM_CHAT_ID=
ANTHROPIC_API_KEY=
CONTROL_API_TOKEN=          # random 32 bytes
```

## 3. Build
```bash
git clone <repo> saarthi && cd saarthi
cargo build --release --workspace
cd strategist && uv sync && cd ..
```

Key dependencies (workspace `Cargo.toml`):
```toml
[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = { version = "0.24", features = ["rustls-tls-webpki-roots"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
rusqlite = { version = "0.37", features = ["bundled"] }
polars = { version = "0.44", features = ["parquet", "lazy"] }
axum = "0.7"
totp-rs = "5"
chrono = "0.4"
chrono-tz = "0.10"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json"] }
plotters = "0.3"
thiserror = "1"
anyhow = "1"
async-trait = "0.1"
```
Strategist (`strategist/pyproject.toml`, locked in `uv.lock`): `anthropic`, `httpx`, `polars`, `feedparser`, `pydantic`,
`jsonschema`. It uses the Anthropic Python SDK's Messages API with its own tool loop rather than the Claude Agent
SDK: the strategist needs only its own read-only tools plus web search, and leaving out the Agent SDK's built-in
shell and file tools means a prompt-injected headline cannot run commands on the trading box.

## 4. Data bootstrap
```bash
./target/release/fetch instruments            # instrument master → data/instruments.parquet
./target/release/fetch candles --universe config/universe.toml --tf 5m --days 365
./target/release/fetch candles --universe config/universe.toml --tf 1d --days 730
```
(Respect SmartAPI historical rate limits; the fetcher throttles and resumes.)

## 5. Test
```bash
cargo nextest run --workspace            # unit + integration
cargo test -p saarthi-risk -- breach     # every hard limit has a breach test
cargo test -p saarthi-oms -- crash_recovery   # real kill -9 of a child process mid-trade
cd strategist && uv run pytest           # plan schema, retry/fallback loop, tools, history scan
```
Required test suites before live:
- `risk`: each limit breached → order blocked; daily loss → flatten + halt
- `oms`: partial fills, rejects, SL placement, square-off retry until flat
- `plan`: malformed/oversized/unknown-symbol plans → clamped or rejected
- `feed`: disconnect → stale → entries blocked → reconnect

## 6. Backtest
```bash
./target/release/backtest --strategy orb --from 2025-01-01 --to 2026-06-30 \
  --walk-forward 60:20 --costs config/costs.toml --out data/reports/bt_orb
```
Gate before enabling a strategy: OOS profit factor > 1.3, max DD < 8%, ≥ 200 trades, stable across parameter neighbourhood.
`--strategy all` runs every strategy; `--fail-on-gate` exits 2 unless all pass; `--walk-forward off --params k=v,...` does a
single in-sample run. Thresholds: `config/backtest.toml`. To test the pipeline without data:
`backtest synth --data-dir /tmp/syn` (reports on synthetic data are labelled as such and mean nothing).

## 7. Run modes
```bash
SAARTHI_MODE=paper ./target/release/engine --config config/engine.toml run
./target/release/engine exit-check --days 5                                   # M3 exit
SAARTHI_MODE=live  ./target/release/engine --config config/engine.toml   # only after M5
uv run --project strategist strategist/agent.py premarket|review|hourly|eod   # --dry-run submits nothing
uv run --project strategist strategist/agent.py validity --runs 20            # M4 exit: 20/20 valid plans
```

## 8. Deploy (VPS, unattended)
```bash
sudo ./deploy/provision.sh   # user 'saarthi', chrony, ufw, logrotate, dirs, copies units
```
systemd units (`deploy/systemd/`):

| Unit | Schedule (IST, Mon–Fri) | Does |
|---|---|---|
| `saarthi-engine.service` | started by timer 08:30, `Restart=always` until 15:45 | boot checks, login, trade, square-off |
| `saarthi-premarket.timer` | 08:45 | `agent.py premarket` |
| `saarthi-review.timer` | every 15 min 09:30–14:45 | `agent.py review` |
| `saarthi-report.timer` | 10:15, 11:15, 12:15, 13:15, 14:15 | hourly report |
| `saarthi-eod.timer` | 15:35 | `agent.py eod` → EOD report |
| `saarthi-stop.timer` | 15:45 | stop engine, backup DB + logs |

Example timer:
```ini
# saarthi-premarket.timer
[Timer]
OnCalendar=Mon..Fri 08:45 Asia/Kolkata
Persistent=false
[Install]
WantedBy=timers.target
```
The engine exits immediately on NSE holidays (`config/holidays.toml`, refreshed yearly), so timers can stay simple.

```bash
sudo systemctl enable --now saarthi-*.timer
journalctl -u saarthi-engine -f
```

## 9. Laptop mode (dev / paper only)
Same units work on Linux. On macOS, use `launchd` plists; on Windows, use WSL2 + systemd. Disable sleep, use wired power and Ethernet. Do **not** run live from a laptop.

## 10. Go-live checklist
- [ ] 10 unattended paper days passed (M5)
- [ ] Static IP registered with Angel One; algo/order-rate compliance verified
- [ ] `risk.toml` halved for the first 20 live days
- [ ] Telegram `/halt` tested end-to-end
- [ ] Broker-side SL verified on a live 1-share test trade
- [ ] Backups and log retention verified
