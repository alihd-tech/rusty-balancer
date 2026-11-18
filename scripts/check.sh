#!/usr/bin/env bash
set -euo pipefail
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -p rusty-balancer -- validate
cargo run -p rusty-balancer -- --config configs/traffic.example.toml validate

# Optional TLS smoke checks: require curl and openssl.
if [[ "${TLS_SMOKE:-0}" == 1 ]]; then bash scripts/tls-smoke.sh; fi
