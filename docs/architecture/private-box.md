# Architecture Slice: Private Box Utility

## Scope

Describes the optional crypto helper for sealed multi-recipient payloads.

## Primary Module

- `src/crypto/private_box.rs`

## Core Design

- One encrypted body is produced for all recipients.
- Per-recipient key material is wrapped using DH-derived secrets.
- Recipients attempt decrypt against slots without cleartext recipient metadata.

## Invariants

- Maximum recipient count is bounded by the utility envelope format.
- Utility is library-level and not part of default feed replication semantics.
- No dedicated REST or MCP route currently exposes this primitive directly.

## Related Usage Doc

- `docs/features/private-box.md`
