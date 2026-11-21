# Rusty Balancer

**A modular reverse proxy and load balancer built in Rust.**

[![Rust CI](https://github.com/alihd-tech/rusty-balancer/actions/workflows/ci.yml/badge.svg)](https://github.com/alihd-tech/rusty-balancer/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white)
![Version](https://img.shields.io/badge/version-0.1.0-blue)
![Status](https://img.shields.io/badge/status-development-orange)

Rusty Balancer is a standalone network traffic engine for routing, distributing, and serving application traffic. It brings together an **HTTP reverse proxy**, **application load balancer**, and **static web server** in a modular Rust workspace, with optional HTTPS termination, TCP/UDP forwarding, and experimental HTTP/3 ingress.

Designed as an independent data plane, Rusty Balancer emphasizes explicit configuration, bounded resource usage, predictable upstream selection, and a locally secured management interface. It can run on its own or serve as a foundation for integration with a broader infrastructure control plane.

> [!IMPORTANT]
> **Development release — v0.1.0.** The implementation and automated verification cover core functionality, but Rusty Balancer has **not** been independently security-audited or certified for production use. Do not expose this development baseline as an untrusted public edge without additional operational validation and hardening. See [Security and limitations](#security-and-limitations).

## Overview

| Capability | What it provides |
| --- | --- |
| **HTTP traffic routing** | Host-based virtual hosts, longest-prefix path routing, streamed reverse proxying, and static file serving |
| **Load balancing** | Round robin, weighted round robin, weighted least-connections, and consistent hashing |
| **Upstream availability** | Configurable active health probes, passive failure ejection, and selection of eligible upstreams for subsequent requests |
| **TLS and HTTPS** | Rustls TLS termination, SNI-based certificate selection, verified HTTPS upstream connections, and optional ACME TLS-ALPN-01 automation |
| **Traffic policies** | CIDR access control, explicitly trusted proxy addresses, rate limiting, connection/request limits, body limits, compression, and opt-in public caching |
| **Real-time connections** | HTTP/1.1 WebSocket proxying, optional TCP forwarding and TLS passthrough, and bounded UDP sessions |
| **Operations** | Authenticated loopback management API, configuration reload/rollback, endpoint inventory, health/readiness checks, and Prometheus-format metrics |
| **Modern protocols** | HTTP/1.1 and HTTP/2 ingress, plus **experimental** HTTP/3 over QUIC with manual certificates |

### Design goals

- **Modular by design.** Networking, routing, load balancing, TLS, middleware, control, and observability are implemented as separate crates.
- **Explicit and reviewable.** Strict TOML validation rejects unknown configuration fields and invalid listener or policy settings.
- **Resource-aware.** Bounded accepted sockets, request admission, transport sessions, rate-limit state, and cache size help constrain resource consumption.
- **Operationally controlled.** Changes to routes, pools, and eligible runtime settings can be applied through a revision-checked local control API; listener-level changes require restart.
- **Independent deployment.** Rusty Balancer does not require a platform controller to serve traffic.

## Quick start

### Requirements

- **Rust and Cargo 1.85 or newer** (current stable recommended)
- **Git** to clone the repository
- **curl** for the HTTP examples
- **Python 3** only for the optional demo backend and selected integration tests

### 1. Clone and validate

```bash
git clone https://github.com/alihd-tech/rusty-balancer.git
cd rusty-balancer

cargo run --locked -p rusty-balancer -- validate
```

The default configuration is [`configs/balancer.toml`](configs/balancer.toml). It listens on `127.0.0.1:8080`, serves the included static example at `/`, and forwards `/api` to a local backend pool.

### 2. Start the server

```bash
cargo run --locked -p rusty-balancer -- serve
```

In another terminal:

```bash
curl http://127.0.0.1:8080/
```

You should receive the included example page. The `/api` route needs the demo backends described below.

### 3. Try load balancing

Start each backend in a separate terminal:

```bash
python3 scripts/demo-backend.py --port 9001
```

```bash
python3 scripts/demo-backend.py --port 9002
```

With Rusty Balancer running, send requests through the upstream pool:

```bash
for i in 1 2 3 4 5 6; do
  curl -s http://127.0.0.1:8080/api
  echo
done
```

The default example assigns weights **2:1** to the two backends and enables active health checks. Newly started backends are eligible after successful probes; the proxy can return `503 Service Unavailable` while no backend is healthy.

To use a different configuration:

```bash
cargo run --locked -p rusty-balancer -- --config configs/traffic.example.toml validate
cargo run --locked -p rusty-balancer -- --config configs/traffic.example.toml serve
```

## Configuration

Rusty Balancer uses a single TOML configuration file. The following is a minimal example of HTTP proxying, health checks, and a static fallback:

```toml
listen = "127.0.0.1:8080"
upstream_timeout_ms = 5000

[pools.app]
algorithm = "weighted_round_robin"

[[pools.app.endpoints]]
url = "http://127.0.0.1:9001"
weight = 2

[[pools.app.endpoints]]
url = "http://127.0.0.1:9002"
weight = 1

[pools.app.health]
path = "/"
interval_ms = 5000
timeout_ms = 1000
healthy_threshold = 1
unhealthy_threshold = 2

[[routes]]
path_prefix = "/api"
pool = "app"

[[routes]]
path_prefix = "/"
static_dir = "examples/public"
```

Routes can match a specific host and/or path prefix. Requests to upstreams retain their original path and query string. When a pool has no eligible endpoints, HTTP requests receive `503`. Failover applies to **new selections**, rather than automatically replaying an already forwarded request.

Additional configuration examples are available for [traffic policies](configs/traffic.example.toml), [manual TLS](configs/tls.example.toml), [management](configs/admin.example.toml), [TCP/UDP](configs/transport.example.toml), and [HTTP/3](configs/http3.example.toml).

### Traffic and security controls

The optional `[traffic]` section defines global policy settings. Route-level policies can configure access rules and request limits.

```toml
[traffic]
max_connections = 4096
max_in_flight = 1024
response_idle_ms = 30000
trusted_proxies = []
```

The accepted-connection budget is shared across HTTP and HTTPS, while `max_in_flight` limits admitted requests and upgraded tunnels. Client IP forwarding is trusted **only** from explicitly configured proxy CIDRs. Configure trusted proxies narrowly and keep any static-file root read-only to untrusted processes.

For rate limiting, cache eligibility, WebSocket settings, response compression, and other policy details, see [Traffic policies](docs/traffic.md).

## TLS, HTTP/3, and network transports

**HTTPS** is optional. Manual certificates use Rustls with SNI-based selection; ACME mode supports TLS-ALPN-01 issuance and renewal for explicitly configured domains. The project also supports verified HTTPS connections to origin servers and optional HTTP-to-HTTPS redirects. See [TLS and certificate management](docs/tls.md).

**HTTP/3** is experimental. It runs over QUIC with TLS 1.3 and currently requires manual certificates. It does not yet support ACME-managed QUIC certificates, automatic Alt-Svc advertisement, or certificate hot reload. See [Network transports](docs/transport.md).

**TCP and UDP** forwarding are separate, opt-in listeners. TCP includes optional SNI passthrough routing and outbound PROXY protocol v1. UDP forwarding is session-based and should only be enabled behind restrictive network policies. See [TCP/UDP example configuration](configs/transport.example.toml).

## Management and observability

An **optional, authenticated management API** runs on a separate **loopback-only** listener. It provides:

- Health and readiness endpoints, upstream/route inventory, and Prometheus-format metrics
- Revision-checked in-memory configuration updates, reload, and rollback
- Cache purging and manual TLS certificate reload

The control API is **disabled by default** and requires a private bearer-token file when enabled. Never expose it directly to the public internet.

See [Management API documentation](docs/management.md) for setup, authentication, endpoints, and the distinction between hot-reloadable and restart-only settings.

## Deployment

### Docker Compose

The included Compose configuration builds the image locally and publishes HTTP **only on the host loopback interface**:

```bash
docker compose -f deploy/compose.yaml up --build
curl http://127.0.0.1:8080/
```

The sample image runs as an unprivileged user with a read-only filesystem. It uses a container-specific static configuration; adapt networking, upstream addresses, certificates, volumes, and ports before deploying custom services. In a container, `127.0.0.1` refers to the container itself, not the host.

### Linux service

A systemd deployment example is included under [`deploy/systemd/`](deploy/systemd/). Use a dedicated service account and review the file, certificate, firewall, and resource-limit requirements before use.

See [Release and deployment](docs/release.md) for packaging, checksums, runtime examples, and benchmark tooling. **No production-ready public release is claimed.**

## Project structure

```text
crates/
  balancer-cli/             CLI, listener startup, shutdown
  balancer-config/          Strict TOML parsing and validation
  balancer-http/            HTTP routing and runtime control
  balancer-proxy/           HTTP upstream proxy and WebSocket relay
  balancer-lb/              Upstream selection and health state
  balancer-tls/             Rustls, certificates, and ACME
  balancer-middleware/      Access rules, rate limiting, caching
  balancer-api/             Authenticated local management API
  balancer-observability/   HTTP metrics and telemetry
  balancer-transport/       TCP and UDP forwarding
  balancer-http3/           Experimental HTTP/3 over QUIC
configs/                    Example TOML configurations
deploy/                     Docker and systemd deployment examples
docs/                       Design, security, and operational guides
scripts/                    Tests, smoke checks, packaging, benchmarks
examples/                   Demo backend and static website
```

## Development and verification

The repository includes a committed `Cargo.lock` and GitHub Actions workflows for formatting, locked compilation, strict Clippy, workspace tests, integration smoke checks, HTTP/3, and RustSec advisory scanning.

```bash
cargo fmt --all -- --check
cargo check --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

For the broader release-candidate gate:

```bash
bash scripts/release-check.sh
```

The full gate requires additional tools, including OpenSSL, Python, and an HTTP/3-capable client (either a suitable curl build or the `aioquic` Python package). See [Release and deployment](docs/release.md). A passing local test suite is **not** a substitute for deployment-specific load, fault, or security testing.

## Documentation

| Guide | Contents |
| --- | --- |
| [Architecture](docs/architecture.md) | Request flow, component boundaries, routing and proxy behavior |
| [Traffic policies](docs/traffic.md) | Limits, access controls, WebSockets, compression and caching |
| [TLS and certificates](docs/tls.md) | Manual PEM certificates, ACME, SNI and redirects |
| [Management API](docs/management.md) | Authentication, metrics, readiness and live configuration |
| [Network transports](docs/transport.md) | TCP/UDP forwarding, TLS passthrough and experimental HTTP/3 |
| [Security](docs/security.md) | Threat assumptions, hardening guidance and current limitations |
| [Release and deployment](docs/release.md) | CI, smoke tests, Docker, systemd and packaging |
| [Roadmap](docs/roadmap.md) | Milestones and planned platform integration |

## Security and limitations

Rusty Balancer is currently a **development-stage traffic engine**, not a drop-in replacement for a fully hardened public-facing edge proxy.

- Safeguards such as resource caps and static path checks do not replace a restrictive firewall, immutable static roots, proper certificate management, and deployment-specific testing.
- The management plane must remain local and accessible only to trusted administrators.
- HTTP/3 and TCP/UDP capabilities have documented limitations. Some networking and observability features still require further hardening.
- The RustSec audit configuration documents narrowly scoped exceptions for two **unmaintained** upstream dependencies; review [the hardening notes](docs/hardening.md) and [security guidance](docs/security.md) before deployment.

Report security-sensitive findings responsibly through the repository's maintainers rather than posting exploit details publicly.

---

**Rusty Balancer** is being developed as a standalone, modular traffic data plane, with future integration into infrastructure management platforms—including Server OS—as a longer-term direction. See the [roadmap](docs/roadmap.md).
