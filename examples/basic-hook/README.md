# Basic Hook

Minimal bash hook using Claude CLI. Includes per-author rate limiting and response cooldown.

## Features

- Pipes queries to `claude --print -p -`
- Author allowlist support
- **Per-author rate limiting** (sliding 60s window)
- **Response cooldown** (silence window after successful reply)

## Quick Start

1. **Configure egregore**:

   ```bash
   cp config.yaml.example ~/egregore-data/config.yaml
   ```

2. **Run egregore**:

   ```bash
   # See examples/systemd/ for service file
   systemctl --user enable --now egregore
   ```

## Rate Limiting

Prevents flood/loop attacks by throttling per author:

| Variable | Default | Description |
|----------|---------|-------------|
| `HOOK_RATE_LIMIT` | `5` | Max messages per minute per author |
| `HOOK_COOLDOWN_MS` | `30000` | Silence window (ms) after successful response |
| `HOOK_STATE_DIR` | `~/.egregore-hook-state` | State directory for counters |

**How it works:**

- Each author gets a 60-second sliding window
- If author exceeds `HOOK_RATE_LIMIT` queries in that window, further queries are skipped
- After responding, hook enters cooldown and ignores all queries for `HOOK_COOLDOWN_MS`

## Other Options

| Variable | Default | Description |
|----------|---------|-------------|
| `ALLOWLIST_FILE` | `~/.egregore-allowlist` | Author allowlist (one ID per line) |

## Files

```
basic-hook/
├── on-message.sh       # Hook script
├── config.yaml.example # Egregore config
└── README.md
```
