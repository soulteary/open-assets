#!/bin/sh
# Runs the Rust harness only on its explicitly empty disposable Linux host.
set -eu
[ "${OPEN_ASSETS_FRESH_TEST:-}" = 1 ] || { echo 'Set OPEN_ASSETS_FRESH_TEST=1 in a disposable environment.' >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo 'Requires root inside the test environment.' >&2; exit 1; }
cd /app/open-assets
binary=${OPEN_ASSETS_ACCEPTANCE_BINARY:-/app/open-assets/target/release/examples/fresh_stack}
[ -x "$binary" ] || { echo 'Build cargo build --locked --release --bin open-assets --example fresh_stack first.' >&2; exit 1; }
export OPEN_ASSETS_BINARY=${OPEN_ASSETS_BINARY:-/app/open-assets/target/release/open-assets}
exec "$binary"
