# Feature: Consumer Groups

## What This Feature Does

This feature coordinates local workers on one node by:

- Group membership (`join`/`leave`)
- Author-feed assignment across members
- Offset commits with assignment checks

This is local coordination; it does not create cross-node group state.

## When To Use It

Use this when you need to:

- Run multiple local workers against one node
- Avoid duplicate local processing
- Track progress per group and author feed

## Usage Examples

Create a group:

```bash
curl -X POST http://localhost:7654/v1/groups \
  -H 'Content-Type: application/json' \
  -d '{"group_id":"analytics"}'
```

Join member:

```bash
curl -X POST http://localhost:7654/v1/groups/analytics/join \
  -H 'Content-Type: application/json' \
  -d '{"member_id":"@AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.ed25519"}'
```

List members and assignments:

```bash
curl http://localhost:7654/v1/groups/analytics/members
```

Commit offset:

```bash
curl -X POST http://localhost:7654/v1/groups/analytics/offsets \
  -H 'Content-Type: application/json' \
  -d '{
    "author":"@<author>.ed25519",
    "sequence":42,
    "committed_by":"@AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=.ed25519"
  }'
```

## CLI Configuration

No direct CLI flags for group lifecycle or offsets.

## `config.yaml` Configuration

No direct `config.yaml` keys for groups.

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/groups` | List groups |
| `POST /v1/groups` | Create group |
| `GET /v1/groups/:id` | Get group details |
| `DELETE /v1/groups/:id` | Delete group |
| `GET /v1/groups/:id/members` | List members and assignments |
| `POST /v1/groups/:id/join` | Join group |
| `POST /v1/groups/:id/leave` | Leave group |
| `GET /v1/groups/:id/offsets` | Read offsets |
| `POST /v1/groups/:id/offsets` | Commit offset |

## Documentation Gaps

1. No CLI workflow exists for group membership and commits.
2. No MCP tools currently expose group operations.
3. No documented pattern for idempotent consumer restart and catch-up logic by generation.
