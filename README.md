# Rusty Balancer

A modular reverse proxy, load balancer and static web server written in Rust. Independent runtime; designed to grow into an infrastructure data plane.

**Development milestone 0.1.0 (P0–P6):** initial implementation, not production-ready. Compilation and test execution are required before release.

## Quick start

Install Rust with Cargo (1.85 or newer; latest stable recommended), then from the repository root:

```sh
cargo run -p rusty-balancer -- validate
cargo run -p rusty-balancer -- serve
curl http://127.0.0.1:8080/
```

Start two demo HTTP backends in separate terminals:

```sh
python3 scripts/demo-backend.py --port 9001
python3 scripts/demo-backend.py --port 9002
```

With Rusty Balancer running, inspect weighted routing:

```sh
for i in 1 2 3 4 5 6; do curl -s http://127.0.0.1:8080/api; echo; done
```

Stop one backend to observe health-check failover after the configured failure threshold.

## Components

| Crate | Responsibility |
| --- | --- |
| balancer-config | Strict TOML parsing, origin and route validation |
| balancer-lb | Algorithms, health eligibility, request leases |
| balancer-proxy | HTTP forwarding, upstream connection pooling, health probes |
| balancer-http | Listener routing, virtual hosts, static files, HTTPS redirects |
| balancer-tls | Rustls termination, SNI and ACME certificate management |
| balancer-middleware | CIDR policy, cache, rate and resource limits |
| balancer-api | Loopback management API |
| balancer-observability | Metrics and response-header latency |
| rusty-balancer | CLI, logging and graceful shutdown |

## Configuration

See [configs/balancer.toml](configs/balancer.toml). Supported algorithms: round_robin, weighted_round_robin, least_connections, consistent_hash. For virtual hosts add a host field to a route. Health checks are optional and use GET with configurable intervals, timeouts and consecutive-result thresholds. All unhealthy pools return HTTP 503.

Inbound HTTP/1.1 and HTTP/2 (cleartext prior knowledge or TLS ALPN); HTTP/1.1 upstreams over HTTP or verified HTTPS. TLS is optional; see [TLS configuration and ACME](docs/tls.md). P4 adds opt-in HTTP/1.1 WebSockets, rate limiting, caching, compression and resource policies; see [traffic policies](docs/traffic.md). Default development listener is loopback. The Docker image uses a static-only container config listening on 0.0.0.0:8080; mount a custom config for accessible upstreams. Localhost inside a container refers to that container. Publish the example only on loopback: docker run --rm -p 127.0.0.1:8080:8080 rusty-balancer.

## Development

```sh
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

See [architecture](docs/architecture.md) for behavior and limitations, and [roadmap](docs/roadmap.md) for planned milestones. Do not deploy as a public production edge until hardening and test validation are complete.

## Management

Optional authenticated loopback control plane with revision-checked reload, rollback, metrics, upstream/route inventory, cache purge and manual certificate reload. See [management](docs/management.md).

## Network transports

Optional TCP/UDP and experimental HTTP/3 ingress; see [transport configuration](docs/transport.md).
