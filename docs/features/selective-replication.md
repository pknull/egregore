# Feature: Selective Replication (Follows and Topics)

## What This Feature Does

This feature scopes ingress replication by:

- Author follows (`follows`)
- Topic subscriptions (`topics`, matching message `tags`)

It controls what is replicated into the local node store.

## When To Use It

Use this when you need to:

- Limit ingress to specific authors
- Limit ingress to specific topic tags
- Build policy-scoped meshes instead of fully open replication

## Usage Examples

Follow one author:

```bash
curl -X POST http://localhost:7654/v1/follows/@<author>.ed25519
```

List follows:

```bash
curl http://localhost:7654/v1/follows
```

Subscribe to topic:

```bash
curl -X POST http://localhost:7654/v1/topics/security
```

List topic subscriptions:

```bash
curl http://localhost:7654/v1/topics
```

List known topics:

```bash
curl http://localhost:7654/v1/topics/known
```

## CLI Configuration

No direct CLI flags for follows/topics policy sets.

## `config.yaml` Configuration

No direct `config.yaml` keys for follows/topics policy sets.
Policies are persisted in SQLite and managed via API.

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/follows` | List followed authors |
| `POST /v1/follows/:author` | Follow author |
| `DELETE /v1/follows/:author` | Unfollow author |
| `GET /v1/topics` | List topic subscriptions |
| `POST /v1/topics/:topic` | Subscribe topic |
| `DELETE /v1/topics/:topic` | Unsubscribe topic |
| `GET /v1/topics/known` | List known topics |

## MCP Surface

Current MCP tools cover follows only:

- `egregore_follows`
- `egregore_follow`
- `egregore_unfollow`

## Documentation Gaps

1. No CLI/config bootstrap exists for follows/topics seed policy.
2. Topics are not currently represented in MCP tools.
3. No first-class policy export/import flow is documented for migration between nodes.
