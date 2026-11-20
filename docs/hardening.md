# P0/P1 hardening status

The first release gate is **reproducibility**. The generated `Cargo.lock` must be committed.
The branch bootstrap workflow generated and committed a `Cargo.lock` using the Rust toolchain.
On future dependency updates, generate a fresh lockfile using:

```sh
cargo generate-lockfile
git add Cargo.lock && git commit -m "chore: pin Rust dependencies"
```

Rust CI requires locked compilation, formatting, Clippy, tests and RustSec audit. The audit gate denies new advisories. `.cargo/audit.toml` narrowly exempts two upstream **unmaintained-only** warnings with no available fixed version (`chrono` transitively through `rustls-acme`, and the direct `rustls-pemfile` dependency); these are explicitly tracked technical debt and must be removed when the dependencies can be migrated. Security-vulnerability advisories remain blocking.
The HTTP/3 smoke job uses a certificate-verifying `aioquic` test client where an HTTP/3-enabled curl is unavailable; a failed HTTP/3 request remains a CI failure.
Do not consider this change ready for production while these jobs are failing,
missing, or unverified.

P1 adds a public accepted-connection budget, health/readiness distinction,
passive HTTP endpoint ejection, optional explicitly trusted forwarding proxies,
and static-file symlink escape checks. See the config example and security docs.
