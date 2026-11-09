#!/bin/sh
# Extract the trusted locally built Rust binary. Does not start containers,
# initialize services, or rewrite configuration/data.
set -eu
umask 077
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
image=${1:-open-assets:local}
[ ! -L "$root/bin" ] || { echo 'Refusing linked runtime directory.' >&2; exit 1; }
mkdir -p "$root/bin"
chmod 755 "$root/bin"
temporary=$(mktemp "$root/bin/.open-assets.XXXXXXXX")
container=''
cleanup() {
  rm -f "$temporary"
  [ -z "$container" ] || docker rm "$container" >/dev/null
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
container=$(docker create --entrypoint /usr/local/bin/open-assets "$image" --version)
docker cp "$container:/usr/local/bin/open-assets" "$temporary"
chmod 755 "$temporary"
"$temporary" --version | grep '^open-assets [0-9]' >/dev/null
target=$root/bin/open-assets
if [ -e "$target" ] || [ -L "$target" ]; then
  [ -f "$target" ] && [ ! -L "$target" ] || { echo 'Refusing linked/non-regular runtime target.' >&2; exit 1; }
  backup=$root/bin/open-assets.before-$(date +%Y%m%d-%H%M%S)-$$
  (set -C; cat "$target" > "$backup")
  chmod 700 "$backup"
fi
sync "$temporary"
mv -f "$temporary" "$target"
sync "$root/bin"
printf 'Native Rust runtime installed; existing configuration and services retained.\n'
