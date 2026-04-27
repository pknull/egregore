# Changelog

All notable changes to egregore are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project follows [Semantic Versioning](https://semver.org/) post-1.0.

## [2.0.1] - 2026-04-27

### Fixed

- **Refuse to silently mint a new identity when only `secret.key.enc` exists.** The 2.0.0 migration was documentation-only; an operator who upgraded without reading `CHANGELOG.md` would have `Identity::load_or_generate` mint a fresh keypair next to the orphaned encrypted key, silently changing the node's `public_id`. `load_or_generate` now refuses to start in that state, with an `EgreError::Config` whose message lists both safe actions: migrate the encrypted key to plaintext per the 2.0.0 migration steps, **or** delete/move `secret.key.enc` to deliberately start with a fresh identity.

  Known limitation: the check uses `Path::exists()`, which returns `false` for a dangling symlink whose target is unmounted. A symlinked encrypted key on an unmounted volume could still slip past the guard. Treated as an acceptable trade-off for the single-operator deployment model; revisit if real deployments use symlinked key files.

## [2.0.0] - 2026-04-27

### ⚠ Breaking

- **`--passphrase` flag removed.** Encrypted-key support has been deleted entirely: `src/identity/encryption.rs` is gone, the `argon2` dependency has been dropped, and the `prompt_passphrase` / `load_encrypted_identity` code paths in `src/main.rs` are removed. Plaintext `secret.key` with strict owner-only file permissions is now the only key storage path.
- **Operators with only `secret.key.enc` on disk MUST migrate before upgrading.** `Identity::load_or_generate` looks for `secret.key`; if absent, it generates a brand-new keypair, writes it next to the orphaned `secret.key.enc`, and the node now signs under a different `@<base64>.ed25519` public ID. Followers tracking the old ID will stop matching. **Migration**: before upgrading, decrypt your encrypted key on the previous version and place the resulting plaintext at `<data_dir>/identity/secret.key` with mode `0600`. There is no in-binary decryption tool in 2.0.
- **`api_auth_enabled` default flipped from `true` to `false`.** Existing `config.yaml` files with the field set explicitly are unaffected; new configs generated via `--init-config`, and any deployment that relied on the default to require auth, must now set `api_auth_enabled: true` explicitly. Fresh installs on localhost get a token-free API by default.
- **`--network-key` is now optional on the CLI** and has no built-in default. The daemon refuses to start without one — pass it on the CLI or set `network_key` in `config.yaml`. Previous releases used `egregore-network-v1` as a placeholder default; deployments relying on that default must specify it explicitly going forward.
- **`CompositeTransport::publish()` is best-effort fanout.** Once a message is durably stored by the local engine, child transport errors no longer fail the synchronous publish call. Errors are logged with `total_children` / `failed_children` / `message_hash` / first-failure summary and surfaced through health / backlog / retry paths instead. Callers that previously relied on `Err` propagation from `publish` to detect remote-fanout failure should consult `TransportHealth` and bridge backlog metrics.

### Added

- `identity::permissions::validate_private_key()` — refuses to load a private key whose mode allows group/other access on Unix. Wired into `Identity::load_unencrypted()` so startup rejects insecure modes rather than silently continuing.
- `CHANGELOG.md` (this file).

### Removed

- `src/identity/encryption.rs` (151 lines) and its `EncryptedKey` type.
- `argon2` crate dependency.
- `--passphrase` CLI flag and the `load_encrypted_identity()` / `prompt_passphrase()` helpers.

### Changed

- `docs/README.md` restructured as the umbrella mdBook stub for Egregore. Detailed protocol, operations, features, and design material continues to ship from this repo's `docs/`.
- `docs/protocol.md` §1.3 (Key Storage) and §11 (cryptographic primitives table) updated to reflect the file-permission-only model.
- `docs/operations.md` CLI flags table reflects the removed `--passphrase` and the now-required `--network-key`; the "Encrypted Key at Rest" section is gone.
- `docs/features/identity-and-security.md` no longer documents `--passphrase`.
- `docs/features/integration-api-and-mcp.md` documents the new `api_auth_enabled: false` default and the explicit-token requirement when enabling.
- `docs/design/cli-publish.md` reflects that publish does not route through a separate encrypted-identity flow.
- `AGENTS.md` and `CLAUDE.md` module tables list `permissions.rs` instead of the deleted `encryption.rs`.

### Fixed

- `config::tests::yaml_round_trip` no longer flips its `api_auth_enabled` assertion (which had been a collateral edit when the default flipped). The test parses an explicitly-`true` round-trip and the assertion remains `true`.

## [1.2.4] - prior

Earlier history is preserved in `git log` and on existing release tags. Highlights: phase 2 work (bus adapter, bridge mode, observability, cross-transport dedup); phase 1 transport abstraction; long-running implementation of signed feeds, gossip replication, schema registry, retention policies, consumer groups, and the rest of the v1.x feature surface.
