#!/bin/sh
# Run as root. Stop writers before taking application-consistent snapshots.
set -eu
: "${BACKUP_ROOT:?Set a private backup directory on independent storage}"
: "${APP_ROOT:=/app/open-assets}"
: "${DATA_ROOT:=/data/asset-stack}"
umask 077
mkdir -p "$BACKUP_ROOT"
archive="$BACKUP_ROOT/open-assets-$(date +%Y%m%d-%H%M%S).tar.gz"
tar -czpf "$archive" "$APP_ROOT/private" "$APP_ROOT/.env" "$DATA_ROOT"
tar -tzf "$archive" >/dev/null
sha256sum "$archive" > "$archive.sha256"
printf 'Backup created and archive verified. Perform a restore drill before relying on it.\n'
