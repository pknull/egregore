# Architecture: System Baseline

## Runtime Shape

Egregore runs as a single daemon process (`egregore`) with:

- Local API plane on loopback (`127.0.0.1`, default port `7654`)
- Gossip plane on TCP (`0.0.0.0`, default port `7655`)
- Optional discovery plane on UDP (`0.0.0.0`, default port `7656`) and mDNS
- Local SQLite state (`data/egregore.db`)
- One Ed25519 identity per node (`data/identity/`)

Implementation anchor: `src/main.rs`.

## Startup Sequence

1. Load config and identity.
2. Open SQLite store.
3. Optionally start HTTP API on loopback.
4. Start gossip TCP server.
5. Start gossip sync loop (outgoing).
6. Optionally start discovery, mDNS, retention task, and hooks.

## Operating Modes

### Ad-Hoc Mesh (free-form)

- Shared `network_key`
- At least one peer path (static peers, discovery, or mDNS)
- Empty follows and topics policy

Behavior:

- Replicate all authors/topics by default
- Converge via pull sync
- Optional lower-latency push

### Scoped Control Plane (policy-driven)

- Private `network_key` per environment
- Follows/topics policy applied
- Schema contracts + optional strict mode
- Local worker coordination via groups
- Retention and mesh-health observability enabled as needed

Behavior:

- Scoped ingress replication
- Explicit governance and lifecycle controls

## Read Next

- Feature slices index: [README.md](./README.md)
- Usage and examples: [`docs/features/README.md`](../features/README.md)
