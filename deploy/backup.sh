#!/usr/bin/env bash
# Daily backup (15:50 IST, saarthi-backup.service): a consistent SQLite copy
# of every account, the paper account files, journals and reports, packed
# into data/backups/<date>.tar.gz. Keeps BACKUP_KEEP_DAYS (default 30) days.
# With BACKUP_REMOTE set (an rclone remote such as "b2:saarthi-backups"), the
# archive is also copied off the box.
set -euo pipefail
cd "$(dirname "$0")/.."
# Optional settings: BACKUP_REMOTE, BACKUP_KEEP_DAYS.
[[ -r config/backup.env ]] && source config/backup.env
DATA="${SAARTHI_DATA:-data}"
KEEP="${BACKUP_KEEP_DAYS:-30}"
DAY="$(TZ=Asia/Kolkata date +%F)"
OUT="$DATA/backups"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$OUT"

shopt -s nullglob
for db in "$DATA"/accounts/*/*/saarthi.db; do
  rel="${db#"$DATA"/}"
  mkdir -p "$STAGE/$(dirname "$rel")"
  # .backup takes a consistent snapshot even if a writer is still attached.
  sqlite3 "$db" ".backup '$STAGE/$rel'"
  sqlite3 "$STAGE/$rel" "PRAGMA integrity_check;" | grep -qx ok || {
    ./deploy/notify.sh "🚨 backup: integrity check failed for $rel"; exit 1; }
  pj="$(dirname "$db")/paper.json"
  if [[ -f "$pj" ]]; then cp -p "$pj" "$STAGE/$(dirname "$rel")/"; fi
done
for d in journal reports strategist; do
  [[ -d "$DATA/$d" ]] && cp -rp "$DATA/$d" "$STAGE/"
done
cp -p config/*.toml config/*.json "$STAGE/" 2>/dev/null || true

tar -C "$STAGE" -czf "$OUT/$DAY.tar.gz.tmp" .
mv "$OUT/$DAY.tar.gz.tmp" "$OUT/$DAY.tar.gz"
find "$OUT" -name '*.tar.gz' -mtime +"$KEEP" -delete
SIZE="$(du -h "$OUT/$DAY.tar.gz" | cut -f1)"

if [[ -n "${BACKUP_REMOTE:-}" ]]; then
  if rclone copy "$OUT/$DAY.tar.gz" "$BACKUP_REMOTE/" --quiet; then
    echo "backup $DAY ($SIZE) copied to $BACKUP_REMOTE"
  else
    ./deploy/notify.sh "⚠️ backup $DAY saved locally ($SIZE) but the copy to $BACKUP_REMOTE failed"
  fi
else
  echo "backup $DAY ($SIZE) saved locally; set BACKUP_REMOTE to copy it off the box"
fi
