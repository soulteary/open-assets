#!/bin/sh
# Root-only, consistent single-host snapshot. Copy encrypted archives off host.
set -eu
: "${APP_ROOT:=/app/open-assets}"
: "${DATA_ROOT:=/data/asset-stack}"
: "${BACKUP_ROOT:=/data/private-backups/open-assets}"
: "${TRAEFIK_CONFIG_ROOT:?Set the preserved Traefik project path}"
: "${ACME_ROOT:?Set the preserved ACME directory}"
umask 077
mkdir -p "$BACKUP_ROOT"
exec 9>"$BACKUP_ROOT/snapshot.lock"
flock -n 9
cd "$APP_ROOT"
resume() { docker compose start otterio gitea oc webhook publisher >/dev/null; }
trap resume EXIT HUP INT TERM
docker compose stop webhook publisher oc gitea otterio >/dev/null
archive="$BACKUP_ROOT/snapshot-$(date +%Y%m%d-%H%M%S).tar.gz"
tar -czpf "$archive" "$APP_ROOT" "$DATA_ROOT" "$TRAEFIK_CONFIG_ROOT" "$ACME_ROOT"
tar -tzf "$archive" >/dev/null
sha256sum "$archive" > "$archive.sha256"
printf 'Consistent application snapshot verified; transfer encrypted copy off host.\n'
