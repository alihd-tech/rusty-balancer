#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
[[ -f Cargo.lock ]] || { echo 'Missing committed Cargo.lock: run cargo generate-lockfile, review and commit it.' >&2; exit 1; }
cargo fmt --all -- --check
cargo check --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
TLS_SMOKE=1 ADMIN_SMOKE=1 NETWORK_SMOKE=1 HTTP3_SMOKE=1 bash scripts/check.sh
cargo audit --deny warnings
