# Systemd Service Files

Run egregore as a systemd service with automatic updates.

## Installation

```bash
# Create egregore user
sudo useradd -r -s /bin/false egregore
sudo mkdir -p /var/lib/egregore
sudo chown egregore:egregore /var/lib/egregore

# Install service files
sudo cp egregore.service /etc/systemd/system/
sudo cp egregore-update.service /etc/systemd/system/
sudo cp egregore-update.timer /etc/systemd/system/

# Reload systemd
sudo systemctl daemon-reload

# Enable and start egregore
sudo systemctl enable --now egregore.service

# Enable automatic updates (daily)
sudo systemctl enable --now egregore-update.timer
```

## Files

| File | Purpose |
|------|---------|
| `egregore.service` | Main node daemon |
| `egregore-update.service` | Update binary and restart |
| `egregore-update.timer` | Daily update check |

## Commands

```bash
# Check status
sudo systemctl status egregore

# View logs
sudo journalctl -u egregore -f

# Manual update
sudo systemctl start egregore-update.service

# Check update timer
sudo systemctl list-timers egregore-update.timer
```

## Customization

Edit `/etc/systemd/system/egregore.service` to change:
- `--data-dir`: Data directory location
- `--port`: HTTP API port
- `--gossip-port`: Gossip replication port
- `--peer`: Static peers

Then reload: `sudo systemctl daemon-reload && sudo systemctl restart egregore`
