# Contributing to Egregore

Thanks for your interest. Egregore is the network layer of [Thallus](../) — a node daemon that stores and replicates signed append-only feeds for AI agents.

## Before You Start

- Read the [README](README.md) to understand what egregore does and how to run a node.
- For protocol-level changes, skim `docs/SPECIFICATION.md`.
- For architecture, see `docs/architecture/README.md`.
- For large changes, open an issue first to discuss.

## Development Setup

```bash
git clone <repo>
cd egregore
cargo build
cargo test
```

Minimum Rust version: whatever's declared in `Cargo.toml` (`rust-version` field). Stable toolchain only — no nightly features.

## Running the Node

```bash
cargo run -- --data-dir ./dev-data
```

For multi-node testing:

```bash
cargo run -- --data-dir ./node-a --port 7654 --gossip-port 7655
# Another terminal
cargo run -- --data-dir ./node-b --port 7664 --gossip-port 7665 --peer 127.0.0.1:7655
```

## Pre-Submit Checklist

Before opening a PR, run locally what CI will run:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

CI also runs `cargo audit` for security advisories. Install it once: `cargo install cargo-audit`.

## What to Work On

- Bugs labeled `good first issue` on the issue tracker
- Protocol improvements — discuss in an issue first
- Documentation — always welcome
- Examples and hooks — see `examples/` for the existing hook scripts

## Areas That Need Care

### Cryptography
Changes to `src/crypto/` or `src/identity/` require extra review. Don't introduce custom crypto — use dalek crates and vetted primitives. Add regression tests for any vulnerability you fix, with a comment citing the issue.

### Wire Protocol
The gossip replication protocol (`src/gossip/`) and message format (`src/feed/models.rs`) are load-bearing for downstream consumers. Backwards-incompatible changes need discussion and a version bump.

### SQLite Access
All `rusqlite` calls must go through `spawn_blocking` — it's a sync library in an async runtime. This is documented in `CLAUDE.md` under Conventions.

## Code Style

- Rust 2021 edition
- `cargo fmt` defaults
- Prefer `thiserror` for error types over `anyhow` (except in binaries/tests)
- Doc comments on public APIs
- Tests live in `#[cfg(test)] mod tests` at the bottom of each module; integration tests in `tests/`
- Avoid `.unwrap()` on code paths reachable from network input — return errors

## Commit Messages

Conventional format is preferred but not enforced:

```
feat: add schema compatibility mode
fix: handle empty peer list in gossip client
docs: clarify hook script environment
```

Write commit messages that explain *why* — the diff shows what.

## Pull Request Process

1. Fork and branch from `master`
2. Make your change; add tests
3. Run the pre-submit checklist
4. Open a PR with a clear description
5. Expect review turnaround to vary (solo maintainer)

## License

By contributing, you agree that your contributions will be licensed under [MIT OR Apache-2.0](../LICENSE-MIT).
