#!/usr/bin/env bash
# Sends one Telegram message using TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID from
# the environment or config/secrets.env. Used by saarthi-alert@.service and
# backup.sh; never fails the caller.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 0
SECRETS="${SAARTHI_SECRETS:-config/secrets.env}"
get() {
  local v="${!1:-}"
  if [[ -z "$v" && -r "$SECRETS" ]]; then
    v="$(grep -E "^[[:space:]]*(export[[:space:]]+)?$1=" "$SECRETS" | tail -n1 | cut -d= -f2- | sed -e 's/^["'\'']//' -e 's/["'\'']$//')"
  fi
  printf '%s' "$v"
}
TOKEN="$(get TELEGRAM_BOT_TOKEN)"
CHAT="$(get TELEGRAM_CHAT_ID)"
MSG="${1:-Saarthi alert}"
logger -t saarthi "$MSG" 2>/dev/null || true
if [[ -z "$TOKEN" || -z "$CHAT" ]]; then
  echo "notify: Telegram not configured; logged only: $MSG" >&2
  exit 0
fi
curl -fsS --max-time 15 -o /dev/null \
  --data-urlencode "chat_id=$CHAT" --data-urlencode "text=$MSG" \
  "https://api.telegram.org/bot${TOKEN}/sendMessage" || echo "notify: send failed" >&2
exit 0
