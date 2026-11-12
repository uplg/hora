#!/bin/sh
# Smoke-test a built Hora image before it is published: the binary is the one
# the build settings describe, it accepts the example config, and the daemon
# it starts answers. Usage: image-smoke.sh <image>
set -eu
image=${1:?usage: image-smoke.sh <image>}
here=$(cd "$(dirname "$0")/../.." && pwd)
name=hora-smoke-$$
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "1. the bundled SQLite was compiled with the repository's flags"
docker run --rm --entrypoint sh "$image" -c 'grep -q "DEFAULT_MEMSTATUS=0" /usr/local/bin/hora' \
  || { echo "LIBSQLITE3_FLAGS from .cargo/config.toml did not reach the image build" >&2; exit 1; }

echo "2. hora check accepts the example config"
docker run --rm -v "$here/config.example.toml:/etc/hora/config.toml:ro" "$image" check

echo "3. the daemon starts, runs as its own user and answers"
docker run -d --name "$name" -p 127.0.0.1:18787:8787 \
  -v "$here/config.example.toml:/etc/hora/config.toml:ro" "$image" >/dev/null
[ "$(docker exec "$name" id -u)" = 10001 ] || { echo "the daemon does not run as uid 10001" >&2; exit 1; }
code=000
for _ in $(seq 1 60); do
  # 503 while the first checks are still out is an answer too.
  code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18787/healthz || true)
  case "$code" in 200|503) break ;; esac
  docker inspect -f '{{.State.Running}}' "$name" | grep -q true || break
  sleep 1
done
case "$code" in
  200|503) ;;
  *) echo "no answer from /healthz (last: $code)" >&2; docker logs "$name" >&2 || true; exit 1 ;;
esac
page=$(curl -s -o /dev/null -w '%{http_code}' -H 'user-agent: Mozilla/5.0' http://127.0.0.1:18787/)
[ "$page" = 200 ] || { echo "the status page answered $page" >&2; docker logs "$name" >&2 || true; exit 1; }
css=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:18787/assets/hora.css")
[ "$css" = 200 ] || { echo "the stylesheet answered $css" >&2; exit 1; }
echo "ok: $image"
