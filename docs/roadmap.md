# Roadmap

- P0: Workspace, configuration, CLI, listener, logging and signal-based graceful shutdown.
- P1: HTTP/1.1 and cleartext HTTP/2 ingress, exact virtual hosts, path routing, streaming HTTP reverse proxy, static server.
- P2: Round robin, weighted round robin, weighted least-connections, consistent hashing, configurable active health checks and selection failover.
- P3: TLS termination, verified HTTPS upstreams, exact SNI, native ACME TLS-ALPN-01 issuance/renewal and HTTP-to-HTTPS policy (source implemented; verification pending).
- P4: CIDR policy, request/concurrency limits, response idle timeout, rate limiting, compression, conservative cache and HTTP/1.1 WebSocket forwarding (source implemented; verification pending).
- P5: Authenticated loopback API, revision-checked runtime configuration apply/reload/rollback, metrics, access logs, health, cache purge and manual certificate reload (source implemented; verification pending).
- P6: Bounded TCP/UDP, exact SNI TLS passthrough, outbound PROXY v1, experimental HTTP/3 ingress (source implemented; verification pending).
- P7: HTTP resource hardening, loopback integration/benchmark harnesses, strict release gate, candidate archive and checksums, Docker Compose and systemd hardening (tooling implemented; execution and public distribution pending).
- P8: Optional Server OS integration through a stable control interface.

Milestone status is provisional until workspace tests and integration tests pass.
