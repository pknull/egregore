# Feature: Events and Hooks

## What This Feature Does

This feature enables event-driven integrations via:

- SSE stream (`GET /v1/events`)
- Deprecated, default-off subprocess hooks
- Compatibility webhooks awaiting RFC 0003 §6 target conformance

Hooks run on publish/ingest events and can be configured with retries and idempotency.

## Compatibility Status

Subprocess hooks are **DEPRECATED** and disabled by default. An `on_message` handler runs only when the top-level config key `allow_subprocess_hooks: true` is set; each enabled subprocess hook emits a startup warning. Migrate message-triggered automation to structured Servitor work before subprocess-hook removal.

Webhooks remain supported independently of `allow_subprocess_hooks`, but are compatibility-only while awaiting RFC 0003 §6 target conformance. The target requires durable, restart-safe delivery and admission controls that the current in-memory hook tracker does not provide.

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

The CLI flag adds an `on_message` entry but does not enable subprocess execution. Set the top-level gate in `config.yaml`:

```yaml
allow_subprocess_hooks: true
```

Configure multiple hooks in `config.yaml`:

```yaml
allow_subprocess_hooks: true

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
| `--hook-on-message` | Path to a deprecated subprocess hook; requires top-level `allow_subprocess_hooks: true` |
| `--hook-webhook-url` | Compatibility webhook target URL; independent of the subprocess gate |
| `--hook-timeout-secs` | Hook timeout |

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `allow_subprocess_hooks` | Default-off top-level compatibility gate for deprecated `on_message` subprocesses |
| `hooks[].name` | Hook identifier |
| `hooks[].on_message` | Deprecated subprocess path; ignored unless the top-level gate is true |
| `hooks[].webhook_url` | Compatibility webhook URL awaiting RFC 0003 §6 target conformance |
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
