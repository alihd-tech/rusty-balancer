#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
tmp="$(mktemp -d)"
pid=''
cleanup() {
  if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  rm -rf "$tmp"
}
trap cleanup EXIT
umask 077
openssl rand -hex 32 > "$tmp/token"
token="$(cat "$tmp/token")"
cat > "$tmp/config.toml" <<EOF
listen = "127.0.0.1:18081"
[admin]
listen = "127.0.0.1:19090"
token_file = "$tmp/token"
[[routes]]
static_dir = "examples/public"
EOF
cargo build -p rusty-balancer
target/debug/rusty-balancer --config "$tmp/config.toml" validate
target/debug/rusty-balancer --config "$tmp/config.toml" serve >"$tmp/log" 2>&1 &
pid=$!
admin() { curl --noproxy '*' --max-time 5 -sS -H "Authorization: Bearer $token" "$@"; }
base=http://127.0.0.1:19090
for _ in $(seq 1 50); do
  if admin -f "$base/health/live" >"$tmp/live" 2>/dev/null; then break; fi
  kill -0 "$pid" || { cat "$tmp/log"; exit 1; }
  sleep 0.1
done
grep -q '"live":true' "$tmp/live"
[[ "$(curl --noproxy '*' --max-time 5 -s -o /dev/null -w '%{http_code}' "$base/metrics")" == 401 ]]
admin -f "$base/v1/status" | grep -q '"revision":1'
curl --noproxy '*' --max-time 5 -fsS http://127.0.0.1:18081/ | grep -q 'Rusty Balancer'
# The public listener must not expose the admin endpoint.
[[ "$(curl --noproxy '*' --max-time 5 -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18081/v1/status)" == 404 ]]
admin -f "$base/metrics" | grep -q 'rusty_balancer_requests_total'
admin -f -H 'Content-Type: application/json' -d '{"expected_revision":1}' "$base/v1/config/reload" | grep -q '"revision":2'
[[ "$(admin -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' -d '{"expected_revision":1}' "$base/v1/config/reload")" == 409 ]]
admin -f -H 'Content-Type: application/json' -d '{"expected_revision":2}' "$base/v1/config/rollback" | grep -q '"revision":3'
admin -f -H 'Content-Type: application/json' -d '{"expected_revision":3}' "$base/v1/cache/purge" | grep -q '"removed":0'
echo 'Management smoke checks passed'
