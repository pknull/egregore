# Feature: Retention and Lifecycle

## What This Feature Does

This feature controls storage lifecycle through:

- Per-message expiration (`expires_at`)
- Retention policies by scope (global/topic/author/content_type)
- Optional compaction by JSON key
- Tombstones for deletion consistency

## When To Use It

Use this when you need to:

- Bound disk growth
- Keep only recent/critical data
- Apply domain-specific compaction rules

## Usage Examples

Enable retention loop:

```bash
cargo run -- --data-dir ./data
```

With `config.yaml`:

```yaml
retention_enabled: true
retention_interval_secs: 3600
tombstone_max_age_secs: 604800
```

Create a global max-age policy (30 days):

```bash
curl -X POST http://localhost:7654/v1/retention/policies \
  -H 'Content-Type: application/json' \
  -d '{"scope":"global","max_age_secs":2592000}'
```

Create a topic max-count policy:

```bash
curl -X POST http://localhost:7654/v1/retention/policies \
  -H 'Content-Type: application/json' \
  -d '{"scope":{"topic":"logs"},"max_count":1000}'
```

## CLI Configuration

No direct CLI flags for retention policy CRUD.

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `retention_enabled` | Enable periodic retention task |
| `retention_interval_secs` | Cleanup interval |
| `tombstone_max_age_secs` | Tombstone cleanup age |

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/retention/policies` | List policies |
| `POST /v1/retention/policies` | Create policy |
| `DELETE /v1/retention/policies/:id` | Delete policy |

## Documentation Gaps

1. No CLI workflow exists for retention policy management.
2. No MCP tools currently expose retention policy operations.
3. No dedicated retention dry-run endpoint is documented for impact preview.
