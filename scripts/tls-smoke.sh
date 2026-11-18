#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
tmp="$(mktemp -d)"
pid=""
cleanup() { if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi; rm -rf "$tmp"; }
trap cleanup EXIT
umask 077
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost -keyout "$tmp/key.pem" -out "$tmp/cert.pem" >/dev/null 2>&1
cat > "$tmp/tls.toml" <<EOF
listen = "127.0.0.1:18080"
[tls]
listen = "127.0.0.1:18443"
redirect_http = true
[[tls.certificates]]
domains = ["localhost"]
cert = "$tmp/cert.pem"
key = "$tmp/key.pem"
[[routes]]
host = "localhost"
static_dir = "examples/public"
EOF
cargo build -p rusty-balancer
target/debug/rusty-balancer --config "$tmp/tls.toml" validate
target/debug/rusty-balancer --config "$tmp/tls.toml" serve >"$tmp/server.log" 2>&1 &
pid=$!
for _ in $(seq 1 50); do
  if curl --noproxy '*' --silent --fail --cacert "$tmp/cert.pem" --resolve localhost:18443:127.0.0.1 https://localhost:18443/ >"$tmp/body"; then break; fi
  kill -0 "$pid" || { cat "$tmp/server.log"; exit 1; }
  sleep 0.1
done
grep -q 'Rusty Balancer' "$tmp/body"
[[ "$(curl --noproxy '*' -s -o /dev/null -w '%{http_code} %{redirect_url}' -H 'Host: localhost:18080' 'http://127.0.0.1:18080/a?q=1')" == '308 https://localhost:18443/a?q=1' ]]
[[ "$(curl --noproxy '*' -s -o /dev/null -w '%{http_code}' -H 'Host: attacker.test' http://127.0.0.1:18080/)" == '421' ]]
if curl --noproxy '*' -ks --resolve unknown.test:18443:127.0.0.1 https://unknown.test:18443/ >/dev/null; then
  echo 'unknown SNI unexpectedly accepted' >&2; exit 1
fi
echo 'TLS smoke checks passed'
