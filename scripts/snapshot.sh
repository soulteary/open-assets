#!/bin/sh
# Root-only, consistent single-host snapshot. Copy encrypted archives off host.
set -eu
: "${APP_ROOT:=/app/open-assets}"
: "${DATA_ROOT:=/data/asset-stack}"
: "${BACKUP_ROOT:=/data/private-backups/open-assets}"
: "${TRAEFIK_CONFIG_ROOT:?Set the preserved Traefik project path}"
: "${ACME_ROOT:?Set the preserved ACME directory}"
umask 077
# Resolve paths and reject a destination inside its own archive inputs.
APP_ROOT=$(realpath -e "$APP_ROOT")
DATA_ROOT=$(realpath -e "$DATA_ROOT")
TRAEFIK_CONFIG_ROOT=$(realpath -e "$TRAEFIK_CONFIG_ROOT")
ACME_ROOT=$(realpath -e "$ACME_ROOT")
mkdir -p "$BACKUP_ROOT"
BACKUP_ROOT=$(realpath -e "$BACKUP_ROOT")
for source in "$APP_ROOT" "$DATA_ROOT" "$TRAEFIK_CONFIG_ROOT" "$ACME_ROOT"; do
  case "$BACKUP_ROOT/" in "$source/"*) echo 'Backup destination must be outside archive inputs.' >&2; exit 1;; esac
  [ "$source" != / ] || { echo 'Refusing filesystem root.' >&2; exit 1; }
done
exec 9>"$BACKUP_ROOT/snapshot.lock"
flock -n 9
cd "$APP_ROOT"
# Remember the exact running set. Never start a previously stopped service.
running=$(docker compose ps --status running --services)
resume() {
  if [ -n "$running" ]; then
    # Service names originate from Compose and cannot contain whitespace.
    docker compose start $running >/dev/null
  fi
}
trap resume EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
if [ -n "$running" ]; then
  docker compose stop $running >/dev/null
fi
archive="$BACKUP_ROOT/snapshot-$(date +%Y%m%d-%H%M%S)-$$.tar.gz"
partial="$archive.partial"
# Relative archive members allow safe extraction into an empty drill directory.
tar -C / -czpf "$partial" "${APP_ROOT#/}" "${DATA_ROOT#/}" "${TRAEFIK_CONFIG_ROOT#/}" "${ACME_ROOT#/}"
tar -tzf "$partial" >/dev/null
mv "$partial" "$archive"
sha256sum "$archive" > "$archive.sha256"
printf 'Consistent application snapshot verified; transfer encrypted copy off host.\n'
