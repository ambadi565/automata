#!/usr/bin/env bash
# Provisions an Ubuntu 22.04/24.04 VPS (Mumbai region, static IP) to run
# Saarthi unattended. Safe to re-run: it also updates an existing install.
#
#   sudo ./deploy/provision.sh                 # from a checkout of this repo
#   sudo SAARTHI_REPO=https://github.com/<you>/automata.git ./provision.sh
#
# What it does: creates the `saarthi` user (home /opt/saarthi), installs
# chrony (clock), ufw (SSH only inbound), sqlite3, fonts for charts, Rust
# and uv for that user, builds the release binaries, syncs the strategist,
# caps journald, installs the systemd units and enables the timers.
# What it does NOT do: fill in config/secrets.env, register the static IP
# with Angel One, or enable live trading.
set -euo pipefail

[[ $EUID -eq 0 ]] || { echo "run as root (sudo)"; exit 1; }
APP=/opt/saarthi
USER_=saarthi
SRC="$(cd "$(dirname "$0")/.." && pwd)"
REPO="${SAARTHI_REPO:-}"

log() { printf '\n==> %s\n' "$*"; }

log "packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q build-essential pkg-config git curl ca-certificates chrony ufw sqlite3 \
  fonts-dejavu-core logrotate rclone unattended-upgrades

log "clock: chrony, refuse to trade on > 500 ms drift (engine preflight checks it)"
systemctl enable --now chrony
chronyc -a makestep >/dev/null 2>&1 || true

log "firewall: SSH in, everything out"
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
ufw allow OpenSSH >/dev/null
ufw --force enable >/dev/null

log "user $USER_ (home $APP)"
id "$USER_" >/dev/null 2>&1 || useradd --system --create-home --home-dir "$APP" --shell /bin/bash "$USER_"
install -d -o "$USER_" -g "$USER_" -m 750 "$APP"

log "code"
if [[ -n "$REPO" ]]; then
  if [[ -d "$APP/.git" ]]; then
    runuser -u "$USER_" -- git -C "$APP" pull --ff-only
  else
    runuser -u "$USER_" -- git clone "$REPO" "$APP.tmp" && cp -a "$APP.tmp/." "$APP/" && rm -rf "$APP.tmp"
  fi
elif [[ "$SRC" != "$APP" ]]; then
  # Copy this checkout, keeping data/ and secrets already on the box.
  tar -C "$SRC" --exclude=./target --exclude=./data --exclude=./config/secrets.env \
    --exclude=./strategist/.venv -cf - . | tar -C "$APP" -xf -
fi
chown -R "$USER_:$USER_" "$APP"
install -d -o "$USER_" -g "$USER_" -m 750 "$APP/data"

log "toolchains (rustup, uv) for $USER_"
runuser -u "$USER_" -- bash -lc 'command -v cargo >/dev/null || curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal'
runuser -u "$USER_" -- bash -lc 'command -v uv >/dev/null || curl -fsSL https://astral.sh/uv/install.sh | sh'

log "build"
runuser -u "$USER_" -- bash -lc "cd $APP && cargo build --release --locked -p engine -p fetch -p feedcheck -p backtest -p report"
runuser -u "$USER_" -- bash -lc "cd $APP/strategist && ~/.local/bin/uv sync --locked --no-dev"

log "secrets"
if [[ ! -f "$APP/config/secrets.env" ]]; then
  install -o "$USER_" -g "$USER_" -m 600 "$APP/config/secrets.env.example" "$APP/config/secrets.env"
  TOKEN="$(openssl rand -hex 32)"
  sed -i "s|^CONTROL_API_TOKEN=.*|CONTROL_API_TOKEN=$TOKEN|" "$APP/config/secrets.env"
  echo "created $APP/config/secrets.env (mode 600) with a random CONTROL_API_TOKEN; fill in the rest"
fi
chmod 600 "$APP/config/secrets.env"
chown "$USER_:$USER_" "$APP/config/secrets.env"

log "logs: journald capped at 2 GB, 90 days"
install -d /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/saarthi.conf <<'EOF'
[Journal]
SystemMaxUse=2G
MaxRetentionSec=90day
EOF
systemctl restart systemd-journald

log "systemd units"
install -m 644 "$APP"/deploy/systemd/*.service "$APP"/deploy/systemd/*.timer /etc/systemd/system/
systemctl daemon-reload
for t in engine premarket review report eod backup; do
  systemctl enable --now "saarthi-$t.timer" >/dev/null
done
systemctl list-timers 'saarthi-*' --no-pager

cat <<EOF

Done. Next (manual):
  1. Edit $APP/config/secrets.env (Angel One keys, TOTP secret, Telegram, Anthropic key).
  2. sudo -u $USER_ $APP/target/release/fetch login-check
  3. sudo -u $USER_ bash -lc 'cd $APP && target/release/fetch && target/release/fetch candles --tf 1d --days 730'
  4. sudo -u $USER_ bash -lc 'cd $APP && target/release/engine preflight'
  5. Optional off-box backups: put BACKUP_REMOTE=<rclone remote:path> in $APP/config/backup.env
     and run 'sudo -u $USER_ rclone config'.
Paper trading then starts by itself at 08:30 IST on the next trading day.
EOF
