# Rusty Balancer

A modular reverse proxy, load balancer and static web server written in Rust. Independent runtime; designed to grow into an infrastructure data plane.

**Development milestone 0.1.0 (P0–P2):** initial implementation, not production-ready. Compilation and test execution are required before release.

## Quick start

Install Rust with Cargo (1.85 or newer; latest stable recommended), then from the repository root:

```sh
cargo run -p rusty-balancer -- validate
cargo run -p rusty-balancer -- serve
curl http://127.0.0.1:8080/
```

Start two local HTTP backends on ports 9001 and 9002 before testing the proxy. For a simple demonstration run in two separate terminals from the repo root:

```sh
python3 -m http.server 9001 --bind 127.0.0.1 --directory examples/public
python3 -m http.server 9002 --bind 127.0.0.1 --directory examples/public
```

Requests to /api retain the prefix when forwarded, so these sample backends return 404 unless you create an api resource. Health checks use /, which returns 200. Use a backend exposing /api for successful proxy responses, or set the route prefix to / in a separate proxy-only configuration.

## Components

| Crate | Responsibility |
| --- | --- |
| balancer-config | Strict TOML parsing, origin and route validation |
| balancer-lb | Algorithms, health eligibility, request leases |
| balancer-proxy | HTTP forwarding, upstream connection pooling, health probes |
| balancer-http | Listener routing, virtual hosts, static files |
| rusty-balancer | CLI, logging and graceful shutdown |

## Configuration

See [configs/balancer.toml](configs/balancer.toml). Supported algorithms: round_robin, weighted_round_robin, least_connections, consistent_hash. For virtual hosts add a host field to a route. Health checks are optional and use GET with configurable intervals, timeouts and consecutive-result thresholds. All unhealthy pools return HTTP 503.

Inbound HTTP/1.1 and cleartext HTTP/2 prior knowledge; HTTP/1.1 upstreams. TLS and WebSockets are planned, not implemented. Default listener is loopback. To run in Docker with published ports, supply a config listening on 0.0.0.0:8080 and reachable upstreams; localhost inside a container refers to that container.

## Development

```sh
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

See [architecture](docs/architecture.md) for behavior and limitations, and [roadmap](docs/roadmap.md) for planned milestones. Do not deploy as a public production edge until hardening and test validation are complete.
