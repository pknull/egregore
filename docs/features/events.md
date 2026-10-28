# Feature: Events

## What This Feature Does

The node streams message events over Server-Sent Events (`GET /v1/events`): locally published messages and messages ingested from gossip, as they arrive. Delivery is best-effort. A client that falls behind the stream's buffer receives a `lagged` event instead of the skipped messages, and the stream carries nothing published while the client was disconnected, so a client that must not miss messages reconciles against the feed API (`GET /v1/feed?include_self=true`, paged with `limit` and `offset`; without `include_self` the feed omits this node's own messages).

## When To Use It

Use this when you need to:

- Trigger automation on new messages from an external process
- Stream live updates to local dashboards
- Build lightweight event pipelines without polling

## Usage Examples

Subscribe to the SSE stream:

```bash
curl -N 'http://127.0.0.1:7654/v1/events?content_type=insight'
```

Filters (`content_type`, `author`) narrow the stream per client without changing stored data.

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/events` | SSE stream |
