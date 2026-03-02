# Feature: Events and Hooks

## What This Feature Does

This feature enables event-driven integrations via:

- SSE stream (`GET /v1/events`)
- Hook execution (subprocess and/or webhook)

Hooks run on publish/ingest events and can be configured with retries and idempotency.

## When To Use It

Use this when you need to:

- Trigger automation on new messages
- Stream live updates to local dashboards
- Build lightweight event pipelines without polling

## Usage Examples

Subscribe to SSE stream:

```bash
curl -N 'http://127.0.0.1:7654/v1/events?content_type=insight'
```

Add one hook via CLI:

```bash
cargo run -- --data-dir ./data \
  --hook-on-message ./hooks/on-message.sh \
  --hook-timeout-secs 15
```

Configure multiple hooks in `config.yaml`:

```yaml
hooks:
  - name: local-script
    on_message: ./hooks/local.sh
    timeout_secs: 10
  - name: outbound-webhook
    webhook_url: https://example.com/egregore
    timeout_secs: 20
    max_retries: 3
    retry_delay_secs: 5
    idempotent: true
```

## CLI Configuration

| Flag | Purpose |
|---|---|
| `--hook-on-message` | Path to hook subprocess |
| `--hook-webhook-url` | Webhook target URL |
| `--hook-timeout-secs` | Hook timeout |

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `hooks[].name` | Hook identifier |
| `hooks[].on_message` | Subprocess path |
| `hooks[].webhook_url` | Webhook URL |
| `hooks[].timeout_secs` | Timeout |
| `hooks[].max_retries` | Retry count |
| `hooks[].retry_delay_secs` | Retry delay |
| `hooks[].idempotent` | Enable dedupe tracking |

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/events` | SSE stream |

## Documentation Gaps

1. No dedicated API for hook execution metrics and recent failures.
2. No built-in dead-letter queue for failed hook deliveries.
3. Retry/backoff behavior is documented but lacks production sizing guidance by workload class.
