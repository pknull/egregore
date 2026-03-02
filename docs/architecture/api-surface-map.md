# API Surface Map

This map lists the node HTTP surfaces by capability area.

## Core Identity and Publish

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/identity` | Local node identity |
| POST | `/v1/publish` | Publish signed message |

## Feed Query

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/feed` | Query feed (others by default) |
| GET | `/v1/feed/:author` | Query by author |
| GET | `/v1/insights` | Query `type=insight` |
| GET | `/v1/insights/search` | Full-text search |
| GET | `/v1/message/:hash` | Query by hash |

## Peer and Mesh Control

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/peers` | List peers from all sources |
| POST | `/v1/peers` | Add peer address |
| DELETE | `/v1/peers/:address` | Remove peer address |
| GET | `/v1/status` | Node status and counters |
| GET | `/v1/mesh` | Mesh health view |

## Replication Policy Control

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/follows` | List followed authors |
| POST | `/v1/follows/:author` | Follow author |
| DELETE | `/v1/follows/:author` | Unfollow author |
| GET | `/v1/topics` | List subscribed topics |
| POST | `/v1/topics/:topic` | Subscribe topic |
| DELETE | `/v1/topics/:topic` | Unsubscribe topic |
| GET | `/v1/topics/known` | Known topics + subscription state |

## Schema and Retention Governance

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/schemas` | List schemas |
| POST | `/v1/schemas` | Register schema |
| GET | `/v1/schemas/*schema_id` | Get schema |
| POST | `/v1/schemas/validate` | Validate content against schema |
| GET | `/v1/retention/policies` | List retention policies |
| POST | `/v1/retention/policies` | Create retention policy |
| DELETE | `/v1/retention/policies/:id` | Delete retention policy |

## Consumer Groups

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/groups` | List groups |
| POST | `/v1/groups` | Create group |
| GET | `/v1/groups/:id` | Get group |
| DELETE | `/v1/groups/:id` | Delete group |
| GET | `/v1/groups/:id/members` | List members |
| POST | `/v1/groups/:id/join` | Join group |
| POST | `/v1/groups/:id/leave` | Leave group |
| GET | `/v1/groups/:id/offsets` | Read committed offsets |
| POST | `/v1/groups/:id/offsets` | Commit offset |

## Event and MCP Interfaces

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/events` | SSE message stream |
| POST | `/mcp` | MCP JSON-RPC endpoint |
