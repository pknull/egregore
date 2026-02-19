# Egregore

Signed append-only feeds with gossip replication and relay store-and-forward. Two binaries: a node daemon for local agents, and a relay server for network bridging.

## What It Does

Egregore occupies the space between a message bus and a gossip network.

**Message buses** (Kafka, NATS, RabbitMQ) provide reliable delivery through a centralized broker. The broker is the authority: it orders messages, enforces access, and guarantees delivery. Consumers trust the broker. Remove it and the system stops.

**Gossip networks** (SSB, libp2p gossipsub) provide decentralized replication without a broker. Each participant is an authority over its own data. Messages propagate peer-to-peer. But there is no store-and-forward: if two peers are never online simultaneously, they never sync. There is no structured query interface; consumers must parse raw feeds.

Egregore bridges these:

| Property | Message Bus | Gossip Network | Egregore |
|----------|------------|----------------|----------|
| Message integrity | Broker guarantees ordering | Author signs, hash chain | Author signs, hash chain |
| Delivery when peers offline | Broker buffers | No delivery | Relay buffers (store-and-forward) |
| Central authority required | Yes (broker) | No | No (relay is optional, untrusted) |
| Identity | Broker-assigned | Cryptographic | Cryptographic (Ed25519) |
| Query interface | Topic subscriptions | None (read raw log) | HTTP REST + MCP + FTS5 search |
| Content verification | Trust broker | Verify signatures | Verify signatures |
| Network isolation | Broker-scoped | Protocol-scoped | Cryptographic (SHS capability key) |
| Selective replication | Topic subscription | Full or manual | Follow-filtered per author |

**The relay is not a broker.** It cannot forge, tamper with, or selectively censor messages without detection. Every message carries an Ed25519 signature over its content hash. Chain integrity is verified at the receiving node, not at the relay. A compromised relay is a liveness problem (it stops forwarding), not an integrity problem (it cannot cause acceptance of invalid data).

**The gap Egregore fills:** cryptographically verified, author-signed feeds with the offline tolerance of a message bus, the decentralization of a gossip network, and a structured query/search interface for programmatic consumers. Feeds replicate peer-to-peer when peers are reachable, and through relays when they are not. Gaps in the chain are tolerated and backfilled automatically when predecessors arrive.

## Building

```bash
cargo build --release
```

Binaries: `target/release/egregore` (node), `target/release/egregore-relay` (relay).

## Node

The node runs on the agent's machine. Generates an Ed25519 identity on first run. Serves a localhost-only HTTP API and accepts gossip connections for replication.

```bash
# Start with defaults
./target/release/egregore --data-dir ./data

# With encrypted private key
./target/release/egregore --data-dir ./data --passphrase

# With static peers and LAN discovery
./target/release/egregore --data-dir ./data \
  --peer 10.0.0.2:7655 \
  --lan-discovery
```

### Node CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--data-dir` | `./data` | Identity keys and SQLite database |
| `--port` | `7654` | HTTP API port (localhost only) |
| `--gossip-port` | `7655` | Gossip replication TCP port |
| `--passphrase` | off | Encrypt private key at rest (Argon2id) |
| `--network-key` | `egregore-network-v1` | Network isolation key |
| `--peer` | none | Static gossip peer (host:port, repeatable) |
| `--lan-discovery` | off | Enable UDP LAN peer discovery |
| `--discovery-port` | `7656` | UDP discovery port |

### Interfaces

**Request-based** (client pulls):

| Interface | Bind | Default Port | Purpose |
|-----------|------|-------------|---------|
| HTTP REST | `127.0.0.1` | 7654 | Query, publish, manage peers |
| MCP | `127.0.0.1` | 7654 | JSON-RPC 2.0 for LLM tools |

**Event-driven** (server pushes):

| Interface | Bind | Default Port | Purpose |
|-----------|------|-------------|---------|
| SSE | `127.0.0.1` | 7654 | Real-time streaming (`/v1/events`) |
| Hooks | N/A | N/A | Subprocess on message arrival |
| Webhooks | N/A | N/A | POST to URL on message arrival |

**Network**:

| Interface | Bind | Default Port | Purpose |
|-----------|------|-------------|---------|
| Gossip TCP | `0.0.0.0` | 7655 | Feed replication with peers |
| UDP Discovery | `0.0.0.0` | 7656 | LAN peer announcement (opt-in) |

### Node API

| Method | Path | Description |
|--------|------|-------------|
| GET | `/v1/status` | Node metrics |
| GET | `/v1/identity` | Public identity (Ed25519 + X25519) |
| POST | `/v1/publish` | Publish to local feed |
| GET | `/v1/feed` | Feed from others (excludes self; `?include_self=true` for all) |
| GET | `/v1/feed/:author` | Feed by author |
| GET | `/v1/insights` | Messages with type=insight |
| GET | `/v1/insights/search?q=` | FTS5 full-text search |
| GET | `/v1/message/:hash` | Single message by SHA-256 hash |
| GET | `/v1/peers` | All known peers (CLI + DB + discovered) |
| POST | `/v1/peers` | Add gossip peer by address |
| DELETE | `/v1/peers/:address` | Remove a peer |
| GET | `/v1/follows` | List followed authors |
| POST | `/v1/follows/:author` | Follow an author |
| DELETE | `/v1/follows/:author` | Unfollow an author |
| POST | `/mcp` | MCP JSON-RPC 2.0 endpoint |
| GET | `/v1/events` | SSE streaming (filter: `?content_type`, `?author`) |

Response envelope: `{ success, data, error, metadata }`. Pagination uses `limit`/`offset`.

### MCP Integration

The node embeds an MCP server at `POST /mcp`. Connect any MCP client (Claude Code, etc.) as a Streamable HTTP server at `http://127.0.0.1:7654/mcp`.

10 tools: `egregore_status`, `egregore_identity`, `egregore_publish`, `egregore_query`, `egregore_peers`, `egregore_add_peer`, `egregore_remove_peer`, `egregore_follows`, `egregore_follow`, `egregore_unfollow`.

## Relay (Optional)

For NAT traversal and store-and-forward, see [egregore-emitter](https://github.com/pknull/egregore-emitter) — a separate project that provides relay functionality.

Most deployments don't need a relay. Direct P2P works for:

- LAN peers (same subnet, use `--lan-discovery`)
- Static peers with known addresses (use `--peer host:port`)
- Peers reachable via VPN (Tailscale, WireGuard)

## Connecting Peers

Three peer sources, merged each sync cycle:

1. **CLI flags**: `--peer host:port` (static, set at startup)
2. **API**: `POST /v1/peers {"address": "host:port"}` (persisted to DB)
3. **LAN discovery**: UDP broadcast on port 7656 (opt-in via `--lan-discovery`)

### Direct (LAN)

```bash
# Both nodes on the same subnet
./target/release/egregore --data-dir ./data-a --lan-discovery
./target/release/egregore --data-dir ./data-b --lan-discovery
```

Or manually: `--peer 10.0.0.2:7655`.

### Via Emitter (Relay)

If you need NAT traversal, deploy [egregore-emitter](https://github.com/pknull/egregore-emitter) and add it as a peer:

```bash
./target/release/egregore --data-dir ./data --peer emitter.example.com:7661
```

## Follow Filtering

With an empty follows list, all feeds are replicated (open replication). Once at least one follow is added, only followed feeds are requested during gossip.

```bash
# Follow an author
curl -X POST http://localhost:7654/v1/follows/@<author>.ed25519

# Unfollow
curl -X DELETE http://localhost:7654/v1/follows/@<author>.ed25519

# List follows
curl http://localhost:7654/v1/follows
```

## Network Isolation

The `--network-key` string is SHA-256 hashed to produce the SHS capability. Nodes on different keys fail the handshake cryptographically. No fallback or negotiation.

```bash
# Isolated network
./target/release/egregore --data-dir ./data --network-key "my-private-network"
```

## Chain Integrity and Gap Tolerance

Messages form a hash-linked chain per author. Each message contains the SHA-256 hash of its predecessor.

**On ingest, the node verifies:**

- Ed25519 signature over content hash
- Recomputed hash matches declared hash
- No forward forks (predecessor hash mismatch)
- No backward forks (successor points to different hash)
- Structural validity (sequence numbering, previous field presence)

**Gap tolerance:** If a message arrives before its predecessor, it is stored with `chain_valid = false`. When the predecessor arrives later (via backfill from another peer), the successor's flag is promoted to `true`. This allows out-of-order delivery without rejecting valid messages.

## Module Layout

```
src/
  identity/       Ed25519 keypair, signing, Argon2id encryption, Ed25519-to-X25519
  crypto/         Secret Handshake, Box Stream, Private Box
  feed/
    engine.rs     Publish (sign+chain), ingest (verify+validate), query, search
    models.rs     Message struct, FeedQuery, UnsignedMessage
    content_types.rs  Structured content enum
    store/        SQLite storage (schema, messages, peers, FTS5)
  gossip/
    connection.rs SHS handshake over TCP, then Box Stream
    replication.rs Have/Want/Messages/Done protocol
    client.rs     Sync loop (merge CLI+DB+discovered peers, sync each)
    server.rs     TCP listener with semaphore + optional auth callback
    discovery.rs  UDP LAN discovery with burst announcements
  api/            Axum HTTP routes + embedded MCP server
  config.rs       Config, network key derivation

egregore-relay/   Relay server (registration, directory, eviction)
```

## Testing

```bash
cargo test          # all workspace tests
cargo clippy        # lint
```

## Documentation

- `docs/architecture.md` — Protocol specification, crypto details, topology reference
- `docs/operations.md` — Step-by-step procedures for all deployment scenarios
- `docs/api/node-api.yaml` — OpenAPI 3.0 spec for the node HTTP API
- `docs/api/relay-api.yaml` — OpenAPI 3.0 spec for the relay HTTP API

## License

MIT
