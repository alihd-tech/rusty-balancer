#!/usr/bin/env bash
# Build local candidate artifacts only; does not publish or tag.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v cargo >/dev/null
[[ -f Cargo.lock ]] || { echo 'Generate, review and commit Cargo.lock before packaging.' >&2; exit 1; }
version="$(cargo metadata --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="rusty-balancer"))')"
target="$(rustc -vV | sed -n 's/^host: //p')"
[[ "$target" == *linux* ]] || { echo 'This packaging script supports Linux only.' >&2; exit 1; }
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+.][0-9A-Za-z.-]+)?$ ]] || exit 1
cargo test --locked --workspace
cargo build --locked --release -p rusty-balancer
command -v cargo-audit >/dev/null || { echo 'Install cargo-audit before packaging.' >&2; exit 1; }
cargo audit --deny warnings
name="rusty-balancer-$version-$target"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/$name/bin" "$tmp/$name/docs" "$tmp/$name/deploy" dist
install -m 0755 target/release/rusty-balancer "$tmp/$name/bin/rusty-balancer"
cp README.md Cargo.lock "$tmp/$name/"
cp docs/*.md "$tmp/$name/docs/"
cp -r deploy/systemd "$tmp/$name/deploy/"
mkdir -p "$tmp/$name/configs" "$tmp/$name/examples/public"
for file in balancer.toml container.toml tls.example.toml acme.example.toml traffic.example.toml admin.example.toml transport.example.toml http3.example.toml; do install -m 0644 "configs/$file" "$tmp/$name/configs/$file"; done
install -m 0644 examples/public/index.html "$tmp/$name/examples/public/index.html"
cargo metadata --locked --format-version 1 > "$tmp/$name/dependency-metadata.json"
# Distribute only tracked/configured sample files, not local secrets.
tar -C "$tmp" -czf "dist/$name.tar.gz" "$name"
(cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "Candidate artifacts: dist/$name.tar.gz and checksum (unsigned; not published)."
