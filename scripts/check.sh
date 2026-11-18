#!/usr/bin/env bash
set -euo pipefail
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -p rusty-balancer -- validate
