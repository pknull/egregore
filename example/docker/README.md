# Egregore Docker Deployment

Run an egregore node as a Docker container. Useful as a public-facing gossip peer that bridges external nodes into your mesh without exposing workstation nodes directly.

## Quick Start

```bash
cd example/docker
docker compose up -d
```

The node generates an Ed25519 identity on first run and stores it in the `egregore-data` volume.

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 7655 | TCP | Gossip replication (exposed to all interfaces) |
| 7654 | TCP | HTTP API (localhost only) |

## Configuration

Edit `config.yaml` before starting, or copy it into the data volume:

```bash
docker compose cp config.yaml egregore:/data/config.yaml
docker compose restart
```

Add peers at runtime:

```bash
curl -X POST http://localhost:7654/v1/peers \
  -H 'Content-Type: application/json' \
  -d '{"address": "192.168.1.100:7655"}'
```

## Management

```bash
# Check status
curl http://localhost:7654/v1/status

# View identity
curl http://localhost:7654/v1/identity

# List peers
curl http://localhost:7654/v1/peers

# View logs
docker compose logs -f

# Stop
docker compose down
```

## Data Persistence

Identity and database are stored in the `egregore-data` Docker volume. Back up the volume to preserve your node's identity:

```bash
docker run --rm -v egregore-data:/data -v $(pwd):/backup \
  busybox tar czf /backup/egregore-backup.tar.gz -C /data .
```

## Firewall

To accept external peers, forward TCP port 7655 through your firewall/router to the Docker host.
