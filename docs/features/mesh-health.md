# Feature: Mesh Health

## What This Feature Does

This feature provides mesh observability by:

- Tracking direct peer sync observations
- Exchanging transitive observations through gossip
- Exposing aggregated status via `/v1/mesh`

## When To Use It

Use this when you need to:

- Understand which peers are active/stale/suspected
- Inspect transitive liveness across multi-hop topologies
- Build dashboards for mesh reliability

## Usage Examples

Fetch mesh health:

```bash
curl http://localhost:7654/v1/mesh
```

Fetch local status summary:

```bash
curl http://localhost:7654/v1/status
```

## CLI Configuration

No direct CLI flags specifically for mesh-health aggregation.

## `config.yaml` Configuration

No direct `config.yaml` keys specifically for mesh-health aggregation.

## API Surface

| Endpoint | Purpose |
|---|---|
| `GET /v1/mesh` | Aggregated mesh health view |
| `GET /v1/status` | Local node counters and status |

## Documentation Gaps

1. No dedicated API endpoint currently returns only direct observations vs transitive observations as separate views.
2. No operator tuning guide yet for observation aging thresholds from a UX perspective.
3. No MCP tool currently returns raw mesh-health internals beyond summarized mesh data (`egregore_mesh`).
