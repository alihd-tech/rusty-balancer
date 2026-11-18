# Network transports (P6)

TCP and UDP listeners are optional, configured at startup, and independent of HTTP routes. Listener ports are checked for collisions within each transport. TCP and UDP can share a numeric port. Changes require restart; the HTTP control plane rejects attempts to change these listeners by hot reload.

Each service provides round-robin, weighted round-robin, weighted least-connections and consistent-hash algorithms. Targets are literal socket addresses (no DNS resolution), and source affinity uses the actual peer IP. CIDR allow/deny uses the immediate peer; deny wins. No arbitrary client-specified destinations are allowed.

TCP supports bounded concurrency, connection/ClientHello deadlines, bidirectional relay with shared inactivity watchdog, maximum lifetime, and half-close. For TLS passthrough, add one or more transport.tcp.sni tables with exact host names and targets. Rustls parses a bounded ClientHello without terminating TLS; the original bytes are forwarded unmodified. Unknown SNI uses a configured default target pool or is rejected. Encrypted ClientHello and wildcard routing are not supported. Optional proxy_protocol = true emits PROXY v1 from actual socket addresses: enable only for upstreams configured to understand it; inbound PROXY headers are not trusted. TCP/UDP targets are initially eligible; active probes and retry/failover for L4 are not yet implemented.

UDP creates a connected upstream socket per client address/port and a bounded queue of eight datagrams. Source IP determines the hash selection, and the session holds its upstream lease until expiry. Over-limit or saturated datagrams are dropped. Idle and lifetime deadlines release sockets. Only use on trusted/private networks or with restrictive ACLs to avoid UDP reflection/amplification; do not expose a generic UDP proxy publicly.

## HTTP/3 (experimental)
Enable the http3 table with manual TLS certificates and UDP reachability. See configs/http3.example.toml. It uses Quinn and h3, TLS 1.3, ALPN h3, address validation, disabled early data and migration, and bounded connection/stream/request/body resources. Requests use the existing HTTPS router and traffic policies. HTTP/3 ingress buffers request bodies to a configured cap before dispatch, with a whole-request deadline; response data streams. HTTP/3 CONNECT, WebTransport, request/response trailers, upstream HTTP/3, ACME integration, Alt-Svc auto-advertisement and QUIC certificate hot reload are not implemented. Use a manual certificate with a trusted domain and restart HTTP/3 to refresh it. Direct HTTP/3 clients can connect without Alt-Svc. HTTP/3 forwarding is experimental until end-to-end verification.

Example SNI service (separate TCP listener from the HTTP/TLS server):

```toml
[[transport.tcp]]
name = "tls-passthrough"
listen = "127.0.0.1:9443"
strategy = "round_robin"
[[transport.tcp.sni]]
host = "example.test"
targets = [{address = "127.0.0.1:10443"}]
```

The current management status and metrics cover the HTTP data plane, including dispatched HTTP/3 requests, not TCP/UDP socket counters. Shutdown stops accepting new transport sessions, drains TCP for up to 30 seconds, and closes UDP and QUIC sessions.
