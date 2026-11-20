#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if ! curl -V | grep -q HTTP3; then
  python3 -c 'import aioquic' >/dev/null 2>&1 || {
    echo 'HTTP/3 smoke requires curl HTTP3 or the aioquic Python package' >&2; exit 1;
  }
fi
tmp="$(mktemp -d)"
pid=''
cleanup() { if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi; rm -rf "$tmp"; }
trap cleanup EXIT
umask 077
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost -addext subjectAltName=DNS:localhost -keyout "$tmp/key.pem" -out "$tmp/cert.pem" >/dev/null 2>&1
cat >"$tmp/config.toml" <<EOF
listen = "127.0.0.1:18082"
[tls]
listen = "127.0.0.1:18444"
[[tls.certificates]]
domains = ["localhost"]
cert = "$tmp/cert.pem"
key = "$tmp/key.pem"
[http3]
listen = "127.0.0.1:18444"
[[routes]]
host = "localhost"
static_dir = "examples/public"
EOF
cargo build -p rusty-balancer
target/debug/rusty-balancer --config "$tmp/config.toml" serve >"$tmp/log" 2>&1 &
pid=$!
for _ in $(seq 1 8); do
  if curl -V | grep -q HTTP3; then
    if curl --noproxy '*' --http3-only --connect-timeout 1 --max-time 2 -fsS --cacert "$tmp/cert.pem" --resolve localhost:18444:127.0.0.1 -w '\n%{http_version}\n' https://localhost:18444/ >"$tmp/body"; then break; fi
  elif python3 scripts/http3-client-smoke.py --host localhost --port 18444 --ca-certs "$tmp/cert.pem" >"$tmp/body" 2>"$tmp/client.err"; then
    break
  fi
  kill -0 "$pid" || { cat "$tmp/log"; exit 1; }
  sleep 0.1
done
if ! grep -q 'Rusty Balancer' "$tmp/body" || [[ "$(tail -1 "$tmp/body")" != 3 ]]; then
  echo 'HTTP/3 smoke failed: captured client and server diagnostics:' >&2
  [[ ! -f "$tmp/client.err" ]] || cat "$tmp/client.err" >&2
  cat "$tmp/log" >&2
  exit 1
fi
echo 'HTTP/3 smoke checks passed'
