# Feature: Mesh Replication

## What This Feature Does

This feature synchronizes feeds across peers using:

- Pull protocol (`Have -> Want -> Messages -> Done`)
- Optional persistent push mode for lower latency
- Flow control and connection limits for backpressure handling

## When To Use It

Use this when you need to:

- Share messages across multiple nodes
- Converge state over unreliable networks
- Reduce propagation delay with push mode

## Usage Examples

Connect two nodes with static peers:

```bash
# Node A
cargo run -- --data-dir ./data-a --gossip-port 7655

# Node B
cargo run -- --data-dir ./data-b --gossip-port 7665 --peer 127.0.0.1:7655
```

Run pull-only mode:

```bash
cargo run -- --data-dir ./data --no-push
```

Tune sync interval:

```bash
cargo run -- --data-dir ./data --gossip-interval-secs 60
```

## CLI Configuration

| Flag | Purpose |
|---|---|
| `--gossip-port` | Gossip listener port |
| `--peer` | Static peer list (repeatable) |
| `--gossip-interval-secs` | Pull sync cycle interval |
| `--no-push` | Disable persistent push mode |
| `--max-persistent-connections` | Cap persistent connections |

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `gossip_port` | Gossip listener port |
| `peers` | Static peers |
| `gossip_interval_secs` | Pull sync interval |
| `push_enabled` | Enable/disable push mode |
| `max_persistent_connections` | Persistent connection cap |
| `reconnect_initial_secs` | Backoff initial delay |
| `reconnect_max_secs` | Backoff max delay |
| `flow_control_enabled` | Enable credit-based flow control |
| `flow_initial_credits` | Initial credit grant |
| `flow_rate_limit_per_second` | Per-peer message rate limit |

## API Surface

No dedicated HTTP endpoint controls the replication state machine directly.
Operational control is via peer management and policy endpoints.

## Documentation Gaps

1. No CLI visibility command exists for current push/persistent connection state.
2. No API endpoint currently returns flow-control metrics in a focused shape.
3. Tuning guidance for high-latency/high-loss networks needs more concrete presets.
