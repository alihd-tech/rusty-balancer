# Management plane (P5)

The management listener is disabled by default and must bind to a distinct loopback port. Every defined endpoint, including health and metrics, requires a bearer token. No CORS support; no management endpoints mounted on the data listener. Use SSH tunneling or a separately secured local controller rather than exposing this port remotely. The token is loaded once from a private regular file and compared as a constant-time SHA-256 digest; restart to rotate it. Use a high-entropy randomly generated token.

```sh
mkdir -p state
chmod 700 state
umask 077
openssl rand -hex 32 > state/admin.token
cargo run -- --config configs/admin.example.toml serve
TOKEN="$(cat state/admin.token)"
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:9090/v1/status
```

| Method | Path | Function |
| --- | --- | --- |
| GET | /health/live | Process can respond |
| GET | /health/ready | Every routed pool has at least one verified reachable endpoint |
| GET | /metrics | Prometheus text metrics |
| GET | /v1/status | Revision, readiness, routes, pools, in-flight count |
| GET | /v1/upstreams | Pool endpoint eligibility, observation state, weights and active leases |
| GET | /v1/routes | Route and cache inventory |
| GET | /v1/certificates | Configured TLS names/mode, not live issuance/expiry |
| PUT | /v1/config | Apply complete TOML configuration in memory |
| POST | /v1/config/reload | Reload the original config file from disk |
| POST | /v1/config/rollback | Restore the previous in-memory configuration |
| POST | /v1/cache/purge | Clear all configured caches or one route index |
| POST | /v1/certificates/reload | Reload configured manual PEM files |

Mutations require JSON with expected_revision (initially 1). PUT /v1/config also requires toml as a string. Cache purge optionally accepts route (zero-based index from the same revision). Stale revisions and restart-only modifications are rejected. Each successful configuration change increments revision. Rejections leave the active generation untouched. Example:

```sh
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"expected_revision":1}' http://127.0.0.1:9090/v1/config/reload
```

Config apply and rollback do not write to disk. Reload reads the original file; startup always reads that file. Save desired configuration separately, use atomic rename on disk, then reload. Full configuration update supports adding/removing routes, pools and endpoints without restart. Listener addresses, TLS settings/domain lists, admin configuration, accepted-socket ceiling and global request-concurrency ceiling are restart-only; the separate manual certificate reload validates new contents at existing PEM paths before swapping the TLS config. ACME remains automatic. Config rollback does not roll back on-disk PEM contents.

Each incoming dispatch snapshots an immutable generation; an in-progress request completes on its original generation. New requests see the new generation. A shared semaphore prevents reload bypassing concurrency limits. A second reload is rejected while the previous generation still has dispatch references. Existing response streams and WebSocket tunnels retain their leases without retaining the full generation. State for health, rate limits and cache resets with each generation; readiness requires observed upstream reachability; pools with health checks start ineligible until their configured healthy threshold is met, and pools without probes remain unready until a successful forwarded response is observed. Rollback is one previous configuration (a subsequent rollback toggles); not a persisted multi-version history. Cache purge uses an epoch fence so a response fetched before purge cannot repopulate the cache.

Metrics include process-lifetime request/response counters, cancellation count, response-header latency histogram, uptime, current in-flight requests, current upstream eligibility and active leases, current cache entries/bytes/hits/misses, readiness and revision. Cache gauges reset on reload. Metrics exclude separate redirect and management listeners, do not count bytes transferred or final streaming failures, and active upstream gauges exclude retired generations. Readiness is not proof of TLS certificate issuance, static file availability or external reachability. Default access logs include method, status, generation and header latency without URL, IP, token or query. Metrics labels use configured pool names and origins, so protect the endpoint.

Use a trusted local administrator only: the complete TOML API has authority to reconfigure upstreams and static roots. No public exposure, RBAC, remote TLS management transport, or persistent database is implemented. The HTTP/HTTPS accepted-socket cap covers TLS handshake slots; the management socket cap is separate. Comprehensive transport metrics and write-side deadlines remain future work.

Run the local end-to-end smoke test with `ADMIN_SMOKE=1 bash scripts/check.sh` (requires curl and OpenSSL). The authenticated management request budget is eight concurrent requests with a 15-second asynchronous deadline. Never include tokens in logs or public files.

P6 adds restart-only TCP/UDP and HTTP/3 settings. Manual certificate reload affects the TCP HTTPS listener only; restart to change the QUIC certificate. Readiness and metrics do not yet reflect L4 listener health.
