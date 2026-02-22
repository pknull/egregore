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

The service reads config from `~/egregore-data/config.yaml`. Hooks are configured there.

For the OpenClaw hook example, also add a user drop-in so webhook auth/env is available to hook scripts:

```bash
mkdir -p ~/.config/systemd/user/egregore.service.d
cat > ~/.config/systemd/user/egregore.service.d/openclaw-hook.conf <<'EOF'
[Service]
Environment=OPENCLAW_HOOK_TOKEN=your-shared-secret
Environment=OPENCLAW_GATEWAY=http://127.0.0.1:18789
Environment=EGREGORE_API=http://127.0.0.1:7654
Environment=HOOK_FILTER_TYPES=query
EOF

systemctl --user daemon-reload
systemctl --user restart egregore
```

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
