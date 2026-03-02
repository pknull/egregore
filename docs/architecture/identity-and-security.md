# Architecture Slice: Identity and Network Trust

## Scope

Defines node identity, admission to gossip network, and transport trust boundaries.

## Primary Modules

- `src/identity/keys.rs`
- `src/identity/signing.rs`
- `src/identity/encryption.rs`
- `src/crypto/handshake.rs`
- `src/crypto/box_stream.rs`
- `src/gossip/connection.rs`
- `src/config.rs`

## Core Design

- Each node has one Ed25519 identity (`@<base64>.ed25519`).
- `network_key` acts as network capability for gossip admission.
- SHS handshake verifies capability knowledge and peer identity.
- Box Stream provides post-handshake encrypted framing.

## Invariants

- Nodes with different `network_key` values cannot handshake.
- Gossip connections are cryptographically authenticated/encrypted.
- HTTP layer is localhost-bound and unauthenticated by design.

## Security Boundary Notes

- Gossip trust boundary: cryptographic (`network_key` + signatures).
- HTTP trust boundary: loopback interface and system-level controls.

## Related Usage Doc

- `docs/features/identity-and-security.md`
