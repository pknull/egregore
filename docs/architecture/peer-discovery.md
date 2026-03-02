# Architecture Slice: Peer Discovery and Addressing

## Scope

Covers how nodes learn peer addresses and persist them for sync loops.

## Primary Modules

- `src/gossip/discovery.rs`
- `src/gossip/mdns.rs`
- `src/feed/store/peers.rs`
- `src/gossip/client.rs`

## Core Design

Peer addresses are merged from:

- Static config/CLI peers
- API-added peers
- LAN discovery announcements
- mDNS discovery
- Previously known/persisted peers

## Invariants

- Discovery packets are filtered by network discriminator.
- Self-announcements are ignored.
- Discovered addresses are deduplicated before dialing.
- Usable peers are persisted in SQLite for future cycles.

## Storage Ownership

- `peers`, `known_peers`

## Related Usage Doc

- `docs/features/peer-discovery.md`
