# Egregore Protocol Specification

Complete protocol specification for implementing an Egregore-compatible node. This document enables full reimplementation from scratch.

## Overview

Egregore is a decentralized append-only log replication protocol. Each participant has a cryptographic identity (Ed25519 keypair) and maintains a hash-linked feed of signed messages. Nodes replicate feeds with peers over encrypted TCP connections.

**Core principles:**

- Append-only: Messages cannot be modified or deleted once published
- Cryptographically signed: Every message is Ed25519-signed by its author
- Hash-linked: Each message references its predecessor's hash
- Eventually consistent: Nodes converge via periodic gossip replication
- Network-isolated: Different network keys create incompatible networks

---

## 1. Identity

### 1.1 Key Generation

Each node generates an Ed25519 signing keypair on first run.

```
signing_key: Ed25519 SigningKey (32 bytes seed, expands to 64 bytes)
verifying_key: Ed25519 VerifyingKey (32 bytes, derived from signing_key)
```

**Public ID format:**

```
@<base64-standard-encoded-verifying-key>.ed25519

Example: @pQhz2z8K9vxH3jM5nR7kL4wY1bF6gN0cT8aX2dS9eU4=.ed25519
```

### 1.2 X25519 Conversion

For Diffie-Hellman operations (handshake, private messages), Ed25519 keys are converted to X25519 using the standard birational map:

```rust
// Ed25519 signing key → X25519 secret
let hash = SHA512(ed25519_signing_key_bytes[0..32]);
let mut x25519_secret = hash[0..32];
x25519_secret[0] &= 248;   // Clear low 3 bits
x25519_secret[31] &= 127;  // Clear high bit
x25519_secret[31] |= 64;   // Set second-highest bit

// Ed25519 verifying key → X25519 public
// Use edwards_to_montgomery() from curve25519-dalek
let x25519_public = ed25519_verifying_key.to_montgomery().to_bytes();
```

### 1.3 Key Storage

Private keys may be encrypted at rest using Argon2id:

```
salt: 16 random bytes
key = Argon2id(passphrase, salt, m=64MB, t=3, p=1)
nonce: 12 random bytes
ciphertext = ChaCha20-Poly1305(key, nonce, signing_key_bytes)

Storage format (JSON):
{
  "encrypted": true,
  "salt": "<base64>",
  "nonce": "<base64>",
  "ciphertext": "<base64>"
}
```

Unencrypted storage is also supported for automated deployments.

**File permissions:** Private key files MUST be mode 0600 (owner read/write only).

---

## 2. Network Key

The network key partitions networks. Nodes with different network keys cannot communicate.

### 2.1 Key Derivation

The network key string (e.g., `"egregore-network-v1"`) is hashed to produce the 32-byte SHS capability:

```
shs_capability = SHA256(network_key_string)
```

### 2.2 Discovery Discriminator

For LAN discovery, a separate 8-byte discriminator is derived to avoid revealing the actual SHS key over UDP:

```
discriminator = SHA256(SHA256(network_key_string))[0..8]
```

### 2.3 Security Note

The default network key `"egregore-network-v1"` is public. Production deployments SHOULD generate a random network key:

```bash
openssl rand -hex 32  # Generate random key
egregore --network-key "$(cat /path/to/key)"
```

---

## 3. Connection Establishment

### 3.1 TCP Connection

Connections are plain TCP. The initiator (client) connects to the responder (server) on the gossip port (default: 7655).

### 3.2 Secret Handshake (SHS)

After TCP connect, both sides perform a 4-step mutual authentication handshake.

**Security properties:**

- Network key as capability (wrong key → immediate rejection)
- Forward secrecy via ephemeral X25519 DH
- Mutual identity proof via Ed25519 signatures
- Directional session keys prevent reflection attacks

#### Step 1: Client Hello

Client generates ephemeral X25519 keypair and sends:

```
client_hello = [
  client_ephemeral_public_key (32 bytes),
  HMAC-SHA256(network_key, client_ephemeral_public_key) (32 bytes)
]
Total: 64 bytes
```

#### Step 2: Server Hello

Server verifies HMAC (proves client knows network key). If invalid, close connection.

Server generates its own ephemeral keypair and sends:

```
server_hello = [
  server_ephemeral_public_key (32 bytes),
  HMAC-SHA256(network_key, server_ephemeral_public_key) (32 bytes)
]
Total: 64 bytes
```

Both sides now compute the shared secret:

```
shared_ab = X25519_DH(local_ephemeral_secret, remote_ephemeral_public)
```

#### Step 3: Client Authentication

Client derives encryption key and proves identity:

```
encryption_key = SHA256(network_key || shared_ab)

signed_data = network_key || server_ephemeral_public || SHA256(shared_ab)
signature = Ed25519_sign(client_signing_key, signed_data)

plaintext = signature (64 bytes) || client_verifying_key (32 bytes)
nonce = [0u8; 12]
ciphertext = ChaCha20-Poly1305_encrypt(encryption_key, nonce, plaintext)

client_auth = length_prefix (4 bytes, big-endian) || ciphertext
```

#### Step 4: Server Authentication

Server decrypts client auth, verifies signature (proves client owns the claimed identity).

Server proves its own identity:

```
signed_data = network_key || client_ephemeral_public || SHA256(shared_ab)
signature = Ed25519_sign(server_signing_key, signed_data)

plaintext = signature (64 bytes) || server_verifying_key (32 bytes)
nonce = [0u8; 12]  // Same nonce, different key material in practice
ciphertext = ChaCha20-Poly1305_encrypt(encryption_key, nonce, plaintext)

server_auth = length_prefix (4 bytes, big-endian) || ciphertext
```

Client decrypts and verifies.

#### Session Key Derivation

After successful handshake, derive directional keys:

```
// For client:
encrypt_key = SHA256(network_key || shared_ab || "client-to-server")
encrypt_nonce = SHA256(client_ephemeral_public)[0..24]
decrypt_key = SHA256(network_key || shared_ab || "server-to-client")
decrypt_nonce = SHA256(server_ephemeral_public)[0..24]

// For server: swap encrypt/decrypt
```

---

## 4. Box Stream

All post-handshake communication uses Box Stream encrypted framing.

### 4.1 Frame Format

```
Wire frame:
[frame_length (4 bytes, big-endian, max 65536)]
[encrypted_header (34 bytes)]
[encrypted_body (variable, max 4096 bytes ciphertext)]

Header plaintext (18 bytes):
[body_length (2 bytes, big-endian)]
[body_mac (16 bytes)]

Header encryption:
  ciphertext = ChaCha20-Poly1305(key, nonce, header_plaintext)
  // 18 bytes plaintext + 16 bytes auth tag = 34 bytes

Body encryption:
  ciphertext = ChaCha20-Poly1305(key, nonce+1, body_plaintext)
  // MAC is stored in header, not appended
```

### 4.2 Nonce Management

- Initial nonce: from handshake (24 bytes)
- Increment as big-endian integer after each encryption/decryption
- Header and body each consume one nonce (2 nonces per frame)

```rust
fn increment_nonce(nonce: &mut [u8; 24]) {
    for i in (0..24).rev() {
        nonce[i] = nonce[i].wrapping_add(1);
        if nonce[i] != 0 {
            break;
        }
    }
}
```

### 4.3 Goodbye Frame

Clean termination is signaled by an all-zero header plaintext:

```
body_length = 0
body_mac = [0u8; 16]
```

Encrypt and send this header. Recipient recognizes it and closes gracefully.

### 4.4 Size Limits

- Maximum body plaintext: 4096 bytes
- Maximum wire frame: 65536 bytes
- Messages larger than 4096 bytes are fragmented at the application layer

---

## 5. Message Format

### 5.1 Message Structure

```json
{
  "author": "@<base64>.ed25519",
  "sequence": 1,
  "previous": null,
  "timestamp": "2026-02-28T12:00:00Z",
  "content": {
    "type": "insight",
    "title": "Example",
    "observation": "..."
  },
  "relates": null,
  "tags": ["example"],
  "schema_id": "insight@1",
  "trace_id": null,
  "span_id": null,
  "hash": "a1b2c3d4...",
  "signature": "<base64>"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `author` | string | Publisher's public ID |
| `sequence` | u64 | Monotonic counter, starts at 1 |
| `previous` | string? | SHA256 hex of previous message (null for seq 1) |
| `timestamp` | string | RFC 3339 UTC timestamp |
| `content` | object | Arbitrary JSON with `type` field |
| `relates` | string? | Hash of related message (threading) |
| `tags` | string[] | Categorization tags |
| `schema_id` | string? | Schema identifier (e.g., "insight@1") |
| `trace_id` | string? | Distributed tracing ID |
| `span_id` | string? | Distributed tracing span |
| `hash` | string | SHA256 hex of unsigned message |
| `signature` | string | Base64 Ed25519 signature |

### 5.2 Signing Process

1. Construct unsigned message (all fields except `hash` and `signature`)
2. Serialize to canonical JSON (keys sorted, no extra whitespace)
3. Compute hash: `hash = hex(SHA256(canonical_json))`
4. Sign: `signature = base64(Ed25519_sign(signing_key, hash_bytes))`

```rust
// Canonical JSON via serde_json::to_string()
let unsigned = UnsignedMessage { author, sequence, previous, ... };
let canonical_json = serde_json::to_string(&unsigned)?;
let hash_bytes = SHA256(canonical_json.as_bytes());
let hash = hex::encode(hash_bytes);
let signature = base64::encode(signing_key.sign(&hash_bytes).to_bytes());
```

### 5.3 Verification Process

1. Extract `hash` and `signature` from message
2. Reconstruct unsigned message
3. Serialize to canonical JSON
4. Verify: `SHA256(canonical_json) == hash_bytes`
5. Verify: `Ed25519_verify(author_verifying_key, hash_bytes, signature)`

### 5.4 Chain Validation

Messages form a hash-linked chain per author:

```
Message 1: sequence=1, previous=null, hash=H1
Message 2: sequence=2, previous=H1, hash=H2
Message 3: sequence=3, previous=H2, hash=H3
```

**Ingest rules:**

| Condition | Action |
|-----------|--------|
| Signature invalid | REJECT |
| Hash mismatch | REJECT |
| Duplicate (same author+sequence) | REJECT |
| sequence = 0 | REJECT |
| sequence = 1 with previous | REJECT |
| sequence > 1 without previous | REJECT |
| Forward fork (previous exists, hash mismatch) | REJECT |
| Backward fork (successor exists, link mismatch) | REJECT |
| Gap (predecessor missing) | ACCEPT with `chain_valid=false` |
| Valid chain link | ACCEPT with `chain_valid=true` |

**Backfill promotion:** When a missing predecessor arrives, update successor's `chain_valid` to `true`.

---

## 6. Replication Protocol

### 6.1 Message Types

```rust
enum GossipMessage {
    Have { feeds, peer_observations, bloom_summaries, subscribed_topics },
    Want { requests },
    Messages { messages },
    Done,
    Push { message },
    Subscribe { mode },
    SubscribeAck { accepted, mode },
    CreditGrant { amount },
    CreditRequest { suggested },
    FlowControlAck { supported, initial_credits },
}
```

### 6.2 Pull Replication Flow

```
Client                              Server
  |                                   |
  |--- Have {feeds, blooms, topics} -->|  Step 1
  |                                   |
  |<-- Have {feeds, blooms, topics} ---|  Step 2
  |                                   |
  |--- Want {requests} --------------->|  Step 3
  |                                   |
  |<-- Messages {batch} ---------------|  Step 4
  |<-- Messages {batch} ---------------|  (repeat)
  |<-- Done ---------------------------|
  |                                   |
  |<-- Want {requests} ----------------|  Step 5
  |                                   |
  |--- Messages {batch} -------------->|  Step 6
  |--- Messages {batch} -------------->|  (repeat)
  |--- Done --------------------------->|
```

#### Step 1-2: Have Exchange

Each side announces:

- `feeds`: List of `{author, latest_sequence}` pairs
- `bloom_summaries`: Per-feed bloom filters of message hashes
- `subscribed_topics`: Tags to filter messages (empty = all)
- `peer_observations`: Health info about other peers (for mesh visibility)

#### Step 3: Want

Client compares server's Have with local state. For each feed where `server.sequence > local.sequence`, add to Want list:

```rust
Want {
  requests: [
    { author: "@...", after_sequence: 5 },  // "give me seq 6+"
  ]
}
```

**Filtering:**

- Follow filter: Only request feeds from followed authors
- Topic filter: Server only sends messages matching client's subscribed topics
- Bloom filter: Server skips messages client already has (based on bloom)

#### Step 4: Messages

Server sends requested messages in batches:

```rust
Messages { messages: [msg1, msg2, ...] }  // up to 50 per batch
Messages { messages: [...] }
Done
```

**Limits:**

- 50 messages per batch (BATCH_SIZE)
- 10,000 messages per session (MAX_MESSAGES_PER_SESSION)
- ~3800 bytes target per payload (before Box Stream framing)

#### Step 5-6: Reverse Direction

Server sends its Want, client responds with Messages.

### 6.3 Bloom Filters

Bloom filters optimize replication by encoding "what I already have."

**Structure:**

```rust
struct BloomFilter {
    bits: String,      // Base64-encoded bit array
    num_bits: usize,   // Filter size in bits
    num_hashes: usize, // Number of hash functions (1-16)
    count: usize,      // Elements inserted
}
```

**Parameters:**

- Default: 1% false positive rate, max 8KB per feed
- Optimal size: `m = -n * ln(p) / (ln(2)²)`
- Optimal hash count: `k = (m/n) * ln(2)`

**Hash function:**

```rust
fn hash_index(hash: &str, func_index: usize, num_bits: usize) -> usize {
    let mut hasher = SHA256::new();
    hasher.update(hash.as_bytes());
    hasher.update([func_index as u8]);
    let result = hasher.finalize();
    let value = u64::from_le_bytes(result[0..8]);
    (value as usize) % num_bits
}
```

**Usage:**

- Sender checks bloom before offering: if `filter.might_contain(hash)`, skip
- False positives cause redundant skips (inefficiency, not incorrectness)
- False negatives are impossible (correctness guaranteed)

### 6.4 Topic Filtering

Messages can be filtered by tags:

```rust
// In Have message
subscribed_topics: ["rust", "llm"]

// Server only sends messages where:
message.tags.iter().any(|t| subscribed_topics.contains(t))
```

Empty `subscribed_topics` means no filtering (replicate all).

---

## 7. Persistent Connections & Push

### 7.1 Subscription Negotiation

After pull replication completes, optionally upgrade to persistent mode:

```
Client                              Server
  |                                   |
  |  (Have/Want/Messages/Done)        |
  |                                   |
  |--- Subscribe { mode: Persistent } -->|
  |                                   |
  |<-- SubscribeAck { accepted, mode } --|
  |                                   |
  |     [connection stays open]       |
```

### 7.2 Flow Control

Credit-based backpressure prevents overwhelming slow peers.

**Messages:**

```rust
CreditGrant { amount: u32 }    // "You may send N more messages"
CreditRequest { suggested: u32 } // "Please grant me credits"
FlowControlAck { supported: bool, initial_credits: u32 }
```

**Protocol:**

1. After SubscribeAck, exchange FlowControlAck
2. Each side starts with `initial_credits` (default: 100)
3. Sending a Push consumes 1 credit
4. Receiver grants credits when ready (`CreditGrant`)
5. Sender requests credits when low (`CreditRequest`, threshold: 20)

**Atomic credit consumption:**

```rust
// CORRECT: atomic check-and-consume
fn try_send(&self) -> bool {
    self.credits.fetch_update(|current| {
        if current > 0 { Some(current - 1) } else { None }
    }).is_ok()
}

// WRONG: race condition
if self.can_send() {     // Thread A: credits=1
    self.consume_send(); // Thread B also sees 1, both proceed
}
```

**Limits:**

- MAX_CREDITS: 10,000 (prevents overflow attacks)
- Rate limiting: optional token bucket (default: 100/sec + 50 burst)

### 7.3 Push Messages

Real-time message broadcast:

```rust
Push { message: Message }
```

Sent bidirectionally over persistent connections when new messages are published.

### 7.4 Connection Registry

Track active persistent connections:

```rust
// DashMap for concurrent access
registry: DashMap<PublicId, Arc<SecureConnection>>

// Broadcast to all
for conn in registry.iter() {
    conn.send(Push { message }).await;
}
```

### 7.5 Reconnection

Exponential backoff with jitter:

```
initial: 5 seconds
max: 5 minutes
jitter: random factor 0.5-1.0

delay(attempt) = min(initial * 2^attempt * jitter, max)
```

---

## 8. Consumer Groups

Kafka-style coordinated consumption for multi-consumer scenarios.

### 8.1 Concepts

- **Group**: Named collection of consumers
- **Member**: A consumer in a group
- **Generation**: Monotonic counter, incremented on rebalance
- **Assignment**: Which feeds each member is responsible for
- **Offset**: Last processed sequence per feed (cursor)

### 8.2 Round-Robin Assignment

Feeds are distributed to members:

```rust
fn round_robin_assign(members: &[PublicId], feeds: &[PublicId])
    -> Vec<(PublicId, Vec<PublicId>)>
{
    let mut assignments = vec![(member, vec![]); members.len()];
    for (i, feed) in feeds.iter().enumerate() {
        assignments[i % members.len()].1.push(feed);
    }
    assignments
}

// 3 feeds, 2 members:
// Member 0: [feed0, feed2]
// Member 1: [feed1]
```

### 8.3 Join/Leave Protocol

**Join:**

1. Create group if not exists
2. Add member to group_members
3. Increment generation
4. Rebalance (reassign all feeds)
5. Return assigned feeds and generation

**Leave:**

1. Remove member
2. Remove member's assignments
3. Increment generation
4. Rebalance remaining members
5. If no members left, delete group

### 8.4 Offset Commit

```rust
commit_offset(group_id, author, sequence, committed_by) -> Result<GroupOffset>
```

- Verifies committer is assigned the feed (in transaction)
- Only increases offset (MAX semantics)
- Records who committed and when

### 8.5 Heartbeat & Eviction

Members must heartbeat to stay alive:

```rust
heartbeat(group_id, member_id) -> bool  // Update last_heartbeat
evict_stale_members(timeout_secs) -> u64  // Remove dead members
```

Default timeout: 30 seconds. Dead members are evicted and their feeds reassigned.

---

## 9. Discovery

### 9.1 LAN UDP Discovery

Opt-in broadcast on port 7656.

**Announcement format:**

```json
{
  "magic": [69, 71, 82, 69],  // "EGRE"
  "network_key_hash": [8 bytes],  // discriminator
  "gossip_port": 7655,
  "public_id": "@..."
}
```

**Filtering:**

- Ignore packets without correct magic
- Ignore packets with different network_key_hash
- Ignore self-announcements (same public_id)

**Schedule:**

- Initial burst: every 5s for 30s
- Quiet: 5 minutes
- Periodic burst: every 5s for 30s
- Repeat

### 9.2 Peer Sources

Sync loop merges peers from:

1. CLI flags (`--peer`)
2. Database (`peers` table, from API/discovery)
3. Known peers table (from successful connections)

---

## 10. Storage

### 10.1 SQLite Schema

```sql
-- Feed state
CREATE TABLE feeds (
    author TEXT PRIMARY KEY,
    latest_sequence INTEGER NOT NULL
);

-- Messages
CREATE TABLE messages (
    author TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    previous TEXT,
    timestamp TEXT NOT NULL,
    content TEXT NOT NULL,  -- JSON
    relates TEXT,
    tags TEXT NOT NULL,     -- JSON array
    schema_id TEXT,
    trace_id TEXT,
    span_id TEXT,
    hash TEXT NOT NULL UNIQUE,
    signature TEXT NOT NULL,
    chain_valid INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (author, sequence)
);

-- Full-text search
CREATE VIRTUAL TABLE messages_fts USING fts5(
    content, tokenize='porter'
);

-- Peers
CREATE TABLE peers (
    address TEXT PRIMARY KEY,
    last_synced TEXT
);

-- Follows
CREATE TABLE follows (
    author TEXT PRIMARY KEY
);

-- Consumer groups
CREATE TABLE consumer_groups (
    group_id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE group_members (
    group_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    joined_at TEXT NOT NULL,
    last_heartbeat TEXT NOT NULL,
    assignment_generation INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (group_id, member_id)
);

CREATE TABLE group_assignments (
    group_id TEXT NOT NULL,
    member_id TEXT NOT NULL,
    author TEXT NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY (group_id, author)
);

CREATE TABLE group_offsets (
    group_id TEXT NOT NULL,
    author TEXT NOT NULL,
    committed_sequence INTEGER NOT NULL DEFAULT 0,
    committed_at TEXT NOT NULL,
    committed_by TEXT NOT NULL,
    PRIMARY KEY (group_id, author)
);
```

---

## 11. Security Considerations

### 11.1 Network Boundary

- HTTP API binds to localhost only (127.0.0.1)
- Gossip binds to all interfaces (0.0.0.0)
- Network key is the trust boundary for gossip

### 11.2 Cryptographic Choices

| Purpose | Algorithm |
|---------|-----------|
| Identity | Ed25519 |
| Key exchange | X25519 (ephemeral) |
| Symmetric encryption | ChaCha20-Poly1305 |
| Hashing | SHA-256 |
| Key derivation (encryption at rest) | Argon2id |
| HMAC | HMAC-SHA256 |

### 11.3 Attack Mitigations

| Attack | Mitigation |
|--------|------------|
| Replay | Nonce increment, directional keys |
| Reflection | Separate client→server / server→client keys |
| Eavesdropping | Full encryption after handshake |
| Man-in-the-middle | Mutual Ed25519 authentication |
| Network confusion | Network key as capability |
| DoS (credit exhaustion) | MAX_CREDITS cap (10,000) |
| Fork attacks | Hash chain validation |
| Signature forgery | Ed25519 verification |

### 11.4 File Permissions

Private key files MUST have mode 0600. Implementations SHOULD verify and warn if permissions are too open.

---

## 12. Backward Compatibility

### 12.1 Protocol Versioning

Extensions use optional fields with `#[serde(default)]`:

```rust
Have {
    feeds: Vec<FeedState>,
    #[serde(default)]
    bloom_summaries: Vec<FeedBloomSummary>,  // New nodes send, old ignore
    #[serde(default)]
    subscribed_topics: Vec<String>,           // New nodes send, old ignore
}
```

Old nodes ignore unknown fields. New nodes handle missing fields via defaults.

### 12.2 Persistent Mode Fallback

- Old servers close after replication; new clients timeout and fall back to pull
- Old clients close; new servers timeout and close normally
- Pull-based sync continues regardless

---

## 13. Implementation Checklist

### Core

- [ ] Ed25519 identity generation and storage
- [ ] Ed25519 ↔ X25519 conversion
- [ ] SHA-256 hashing
- [ ] ChaCha20-Poly1305 encryption

### Handshake

- [ ] Client hello (ephemeral + HMAC)
- [ ] Server hello (ephemeral + HMAC)
- [ ] Client auth (sign + encrypt)
- [ ] Server auth (sign + encrypt)
- [ ] Session key derivation

### Box Stream

- [ ] Header encryption/decryption
- [ ] Body encryption/decryption
- [ ] Nonce management
- [ ] Goodbye frame
- [ ] Fragmentation (>4096 bytes)

### Messages

- [ ] Canonical JSON serialization
- [ ] Hash computation
- [ ] Ed25519 signing
- [ ] Ed25519 verification
- [ ] Chain validation

### Replication

- [ ] Have/Want/Messages/Done exchange
- [ ] Bidirectional replication
- [ ] Follow filtering
- [ ] Bloom filter support
- [ ] Topic filtering

### Persistence

- [ ] Subscribe/SubscribeAck
- [ ] Push messages
- [ ] Credit-based flow control
- [ ] Connection registry

### Consumer Groups

- [ ] Join/leave with rebalancing
- [ ] Round-robin assignment
- [ ] Offset tracking
- [ ] Heartbeat/eviction

### Discovery

- [ ] UDP announcement format
- [ ] Network key discriminator
- [ ] Self-filtering
- [ ] Burst schedule

---

*Document version: 1.0*
*Protocol version: egregore-network-v1*
