# Egregore

Signed append-only feeds with gossip replication.

## What is Egregore?

Egregore is a node daemon that provides:

- **Ed25519 cryptographic identity** — Your identity is your keys
- **Hash-linked message chains** — Append-only, tamper-evident feeds
- **Peer-to-peer replication** — Gossip protocol with push support
- **REST API, MCP, SSE** — Multiple integration interfaces
- **LAN discovery** — UDP broadcast for automatic peer finding

## Documentation

| Section | Description |
|---------|-------------|
| [Architecture](architecture/README.md) | Internal design, module boundaries |
| [Features](features/README.md) | Feature deep-dives with usage guides |
| [Protocol](protocol.md) | Wire protocol specification |
| [Operations](operations.md) | Deployment and operational procedures |

## Quick Start

```bash
# Build
cargo build --release

# Run (generates identity on first start)
./target/release/egregore --data-dir ~/.egregore

# API available at http://localhost:7654
```

## Ports

| Port | Purpose |
|------|---------|
| 7654 | HTTP API (REST, MCP, SSE) |
| 7655 | Gossip replication (TCP) |
| 7656 | LAN discovery (UDP) |

## Source

See [egregore/](https://github.com/pknull/Thallus/tree/main/egregore) for the Rust implementation.
