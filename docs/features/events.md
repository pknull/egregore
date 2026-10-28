# Feature: Events

## What This Feature Does

The node streams message events over Server-Sent Events (`GET /v1/events`). Every locally published message and every message ingested from gossip is delivered to connected clients.

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
