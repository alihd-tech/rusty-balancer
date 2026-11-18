# Roadmap

- P0: Workspace, configuration, CLI, listener, logging and signal-based graceful shutdown.
- P1: HTTP/1.1 and cleartext HTTP/2 ingress, exact virtual hosts, path routing, streaming HTTP reverse proxy, static server.
- P2: Round robin, weighted round robin, weighted least-connections, consistent hashing, configurable active health checks and selection failover.
- P3: TLS termination, verified HTTPS upstreams, exact SNI, native ACME TLS-ALPN-01 issuance/renewal and HTTP-to-HTTPS policy (source implemented; verification pending).
- P4: CIDR policy, request/concurrency limits, response idle timeout, rate limiting, compression, conservative cache and HTTP/1.1 WebSocket forwarding (source implemented; verification pending).
- P5: Admin API, reload, metrics, tracing and readiness.
- P6: HTTP/3 and L4 TCP/UDP.
- P7: Benchmarks, release packaging and deployment hardening.
- P8: Optional Server OS integration through a stable control interface.

Milestone status is provisional until workspace tests and integration tests pass.
