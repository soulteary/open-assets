#!/bin/sh
# Compatibility entry point: always use the complete, stopped-writer snapshot.
set -eu
: "${BACKUP_ROOT:?Set a private backup directory on independent storage}"
exec sh "$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/snapshot.sh"
