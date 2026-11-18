# Release and deployment (P7)

No release is published automatically. The project remains a 0.1.0 development candidate. Rust/Cargo dependency resolution and verified lockfile are prerequisites.

```sh
cargo generate-lockfile
cargo fmt --all
# review changes and commit Cargo.lock + formatting corrections
cargo install cargo-audit --locked
bash scripts/release-check.sh
bash scripts/package.sh
```

The strict gate runs formatting, locked compilation, Clippy with warnings denied, workspace unit/integration tests, TLS/admin/network/HTTP3 smoke tests and RustSec audit. Requires Rust stable, Python 3, OpenSSL and a curl build with HTTP/3 support. A failure blocks release. Do not skip the HTTP/3 smoke test when shipping HTTP/3 support. Candidate artifacts are generated locally for the host Linux architecture; inspect the archive and checksum before publishing. The script does not create Git tags, releases or container images in a registry.

## Docker
From the repository root:

```sh
docker compose -f deploy/compose.yaml up --build
curl http://127.0.0.1:8080/
```

The image runs as UID 10001. Compose publishes to loopback, drops all capabilities and uses a read-only filesystem. Production certificates and state must be mounted separately. Do not publish the administration listener. Configure UDP mappings explicitly for intended QUIC/L4 listeners.

## systemd
After passing release gates, verify archive checksums and signature/provenance independently. As root, create a dedicated system user/group rusty-balancer, install the reviewed binary at /usr/local/bin/rusty-balancer, copy deploy/systemd/balancer.toml to /etc/rusty-balancer/balancer.toml, and install the static example under /var/lib/rusty-balancer/public. Configure directory ownership and permissions so the service can read it but cannot modify configuration. Install the service at /etc/systemd/system/rusty-balancer.service, then run systemctl daemon-reload and systemctl enable --now rusty-balancer. Verify service logs and loopback connectivity before enabling external listeners.

## Benchmarks
Run the release binary and invoke:

```sh
python3 scripts/benchmark.py --requests 10000 --concurrency 32
```

The loopback HTTP/1.1 harness reports measured throughput, errors and latency p50/p95/p99. Compare static responses and reverse proxy routes with identical payloads, machine configuration, connection reuse and warmup. Record OS, CPU, Rust version, commit, concurrency and raw JSON. No benchmarks have been measured for this milestone. HTTP/2/3 and multi-host workload benchmarks remain future work.
