# Feature: Peer Discovery

## What This Feature Does

This feature finds peer addresses from:

- Static configuration (`--peer`, `peers`)
- LAN UDP discovery
- mDNS discovery
- Persisted peer records in local SQLite

## When To Use It

Use this when you need to:

- Bootstrap local LAN meshes quickly
- Connect over tailnets/VPNs without manual peer lists
- Persist discovered peers for future sync cycles

## Usage Examples

Static peers only:

```bash
cargo run -- --data-dir ./data --peer 10.0.0.2:7655 --peer 10.0.0.3:7655
```

LAN discovery:

```bash
cargo run -- --data-dir ./data --lan-discovery --discovery-port 7656
```

mDNS discovery:

```bash
cargo run -- --data-dir ./data --mdns
```

## CLI Configuration

| Flag | Purpose |
|---|---|
| `--peer` | Add static peer address |
| `--lan-discovery` | Enable UDP LAN discovery |
| `--discovery-port` | Set LAN discovery UDP port |
| `--mdns` | Enable mDNS discovery |

## `config.yaml` Configuration

| Key | Purpose |
|---|---|
| `peers` | Static peer list |
| `lan_discovery` | Enable LAN discovery |
| `discovery_port` | LAN UDP discovery port |
| `mdns_discovery` | Enable mDNS discovery |
| `mdns_service` | mDNS service name |

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/peers` | List known peers |
| `POST /v1/peers` | Add peer |
| `DELETE /v1/peers/:address` | Remove peer |

## Documentation Gaps

1. No dedicated endpoint distinguishes peer source (`static`, `LAN`, `mDNS`, `manual`) in a first-class field.
2. Discovery troubleshooting (firewall, multicast scope, subnet boundaries) is still centralized, not feature-specific.
3. No CLI command exists to print discovery diagnostics without starting full node runtime.
