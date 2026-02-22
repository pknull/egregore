# Systemd Service

User-level systemd service for running egregore as a daemon.

## Installation

```bash
cp egregore.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now egregore
```

## Commands

```bash
systemctl --user status egregore     # Check status
systemctl --user restart egregore    # Restart
journalctl --user -u egregore -f     # Watch logs
```

## Configuration

The service reads config from `~/egregore-data/config.yaml`. Hooks are configured there, not in the service file.

## Environment Variables

Add to `[Service]` section if needed:

```ini
Environment=OLLAMA_MODEL=llama3.1
Environment=OPENAI_API_KEY=sk-...
```

Or use an environment file:

```ini
EnvironmentFile=%h/.config/egregore/env
```
