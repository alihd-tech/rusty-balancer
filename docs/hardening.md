# P0/P1 hardening status

The first release gate is **reproducibility**. The generated `Cargo.lock` must be committed.
On the `hardening/p0-p1-20261121` branch the `Pin Rust dependencies` workflow
attempts to generate and commit the file with the repository's Rust toolchain.
If Actions is disabled or lacks write permissions, run:

```sh
cargo generate-lockfile
git add Cargo.lock && git commit -m "chore: pin Rust dependencies"
```

Rust CI requires locked compilation, formatting, Clippy, tests and RustSec audit.
The HTTP/3 smoke job requires an HTTP/3-enabled curl runner and fails closed otherwise.
Do not consider this change ready for production while these jobs are failing,
missing, or unverified.

P1 adds a public accepted-connection budget, health/readiness distinction,
passive HTTP endpoint ejection, optional explicitly trusted forwarding proxies,
and static-file symlink escape checks. See the config example and security docs.
