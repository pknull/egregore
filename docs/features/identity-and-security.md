# Feature: Identity and Network Trust

## What This Feature Does

This feature defines:

- Node identity (`@<base64>.ed25519`)
- Network isolation boundary (`network_key`)
- Optional private-key encryption at rest (`--passphrase`)

## When To Use It

Use this when you need to:

- Isolate environments (dev/staging/prod) at the gossip layer
- Ensure message authors are cryptographically attributable
- Protect local identity key material on disk

## Usage Examples

Generate identity and run normally:

```bash
cargo run -- --data-dir ./data
```

Encrypt private key at rest:

```bash
cargo run -- --data-dir ./data --passphrase
```

Run a private network (non-default key):

```bash
cargo run -- --data-dir ./data --network-key "my-private-network-2026"
```

Read local public identity:

```bash
curl http://localhost:7654/v1/identity
```

## CLI Configuration

| Flag | Purpose |
|---|---|
| `--passphrase` | Encrypt/decrypt private key at rest |
| `--network-key <key>` | Set gossip network trust boundary |

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `network_key` | Persistent network isolation key |

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/identity` | Return node public identity |

## Documentation Gaps

1. Default `network_key` is public (`egregore-network-v1`); production guidance exists, but automated enforcement does not.
2. HTTP layer auth/TLS is not configured in-process; security relies on localhost binding and external edge controls.
3. Key rotation procedures are not documented as an operator playbook.
