# Feature: Private Box Utility

## What This Feature Does

This feature provides a cryptographic helper module for:

- Multi-recipient payload encryption
- Per-recipient key wrapping using DH-derived secrets
- One encrypted payload body shared across recipients

It is currently a library utility, not a first-class node API feature.

## When To Use It

Use this when you need to:

- Prototype private payload exchange in application code
- Encrypt payloads for multiple recipients outside standard feed routes

## Usage Example (Library-Level)

```rust
use egregore::crypto::private_box::{box_message, unbox_message};

// sender: Identity
// recipients: Vec<X25519Public>
let ciphertext = box_message(&sender, &recipients, b"secret payload")?;
let plaintext = unbox_message(&recipient_identity, &sender_x25519_public, &ciphertext)?;
```

## CLI Configuration

No CLI surface for Private Box.

## `config.yaml` Configuration

No `config.yaml` keys for Private Box.

## API Surface

No HTTP or MCP endpoint currently exposes Private Box directly.

## Documentation Gaps

1. No end-user API route exists for this feature.
2. No operational examples exist for distributing recipient key material.
3. No compatibility/versioning guide exists for payload envelope evolution.
