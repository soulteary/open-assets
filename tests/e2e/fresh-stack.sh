#!/bin/sh
# Use only inside a disposable Linux environment with its own empty Docker daemon.
set -eu
[ "${OPEN_ASSETS_FRESH_TEST:-}" = 1 ] || { echo 'Set OPEN_ASSETS_FRESH_TEST=1 in a disposable environment.' >&2; exit 1; }
[ "$(id -u)" = 0 ] || { echo 'Requires root inside the test environment.' >&2; exit 1; }
cd "$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)"
[ "$(pwd -P)" = /app/open-assets ] || { echo 'Fresh setup recipes require /app/open-assets.' >&2; exit 1; }
[ ! -e private ] && [ ! -e /data/asset-stack ] || { echo 'Refusing existing private configuration or data.' >&2; exit 1; }
[ -z "$(docker ps -aq)" ] || { echo 'Refusing a Docker daemon with existing containers.' >&2; exit 1; }
python3 -c 'import yaml, bcrypt'
NODE_NAME=acceptance-node PUBLIC_BASE=acceptance.invalid python3 scripts/prepare-private.py
python3 scripts/deployment_config.py --env .env --domains private/domains.json
docker compose config --quiet
docker compose pull otterio oc stargate gitea nginx
docker compose build publisher
docker compose up -d --no-build otterio gitea stargate oc
python3 - <<'PY'
import subprocess,time,urllib.request
for _ in range(90):
    try:
        assert urllib.request.urlopen('http://127.0.0.1:13000/api/healthz', timeout=2).status == 200
        assert b'IAM initialization complete' in subprocess.check_output(['docker','compose','logs','otterio'], stderr=subprocess.DEVNULL)
        break
    except Exception:
        time.sleep(1)
else:
    raise SystemExit('Backend readiness timeout; inspect this isolated environment.')
PY
python3 scripts/initialize-services.py
docker compose run --rm --no-deps -v "$PWD/private/generated:/generated" publisher render /generated
docker compose up -d --no-build nginx webhook publisher
docker compose exec -T nginx nginx -c /etc/nginx/open-assets.conf -t
python3 tests/e2e/verify.py

if [ "${OPEN_ASSETS_COMMUNITY_TEST:-}" = 1 ]; then
  python3 tests/e2e/community.py
fi
