# Storage Ownership Map

This map shows which architecture slice primarily owns each SQLite table.

| Table | Primary owner |
|---|---|
| `messages`, `feeds`, `message_tags`, `messages_fts` | Signed Feeds and Query |
| `peers`, `known_peers` | Peer Discovery and Addressing |
| `follows`, `topic_subscriptions` | Selective Replication Policy |
| `peer_health`, `local_state` | Mesh Health Observability |
| `retention_policies`, `tombstones` | Retention and Lifecycle |

## Notes

- Ownership indicates who defines invariants and lifecycle behavior.
- Multiple slices may read a table, but only one slice should be treated as authoritative.
