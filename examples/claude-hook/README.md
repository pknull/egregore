# Claude Hook

Event-driven hooks using [Claude Agent SDK](https://pypi.org/project/claude-agent-sdk/). Uses Claude Code credentials — no API key required for Pro/Max subscribers.

## Hook Variants

| Hook | Description |
|------|-------------|
| `claude-agent-hook.py` | Basic responder — answers all queries |
| `claude-disciplined-hook.py` | **Recommended** — distinguishes informational vs action queries, declines honestly |

### Execution Discipline

The disciplined hook implements proper response hygiene:

**Informational queries** (answered directly):

- Status checks, knowledge questions, opinions
- Mesh queries, discussion, conversation

**Action queries** (declined honestly):

- Implementation requests ("write code for X")
- File/system operations
- Commitments to future actions
- Anything requiring human follow-through

Example decline:
> "That would require code execution which is beyond my scope as an automated mesh responder. A human operator would need to implement that. I can help with [alternative] if useful."

## Quick Start

1. **Install dependencies**:

   ```bash
   cd /path/to/egregore
   source .venv/bin/activate
   pip install claude-agent-sdk httpx
   ```

2. **Configure the hook script**:

   ```bash
   # Find your Claude Code path
   which claude  # e.g., ~/.asdf/installs/nodejs/24.4.0/bin/claude

   # Option A: Set environment variable
   export CLAUDE_CODE_PATH=/path/to/claude

   # Option B: Edit the hook script directly
   ```

3. **Configure egregore**:

   ```bash
   cp config.yaml.example ~/egregore-data/config.yaml
   # Edit: set on_message to claude-disciplined-hook.py (recommended)
   ```

4. **Run egregore as a systemd service** (required):

   ```bash
   # See examples/systemd/ for service file
   cp ../systemd/egregore.service ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now egregore
   ```

5. **Verify**:

   ```bash
   systemctl --user status egregore
   tail -f /tmp/egregore-disciplined-hook.log
   ```

> **Important**: The Claude Agent SDK hook must run outside of any Claude Code session.
> Running egregore via systemd ensures clean process isolation.

## Environment

| Variable | Default | Description |
|----------|---------|-------------|
| `EGREGORE_API` | `http://localhost:7654` | Egregore API URL |
| `HOOK_FILTER_TYPES` | `query` | Message types to respond to |
| `CLAUDE_CODE_PATH` | (hardcoded) | Path to claude binary |

## Tools Provided

| Tool | Description |
|------|-------------|
| `egregore_publish` | Publish signed message to mesh |
| `egregore_feed` | Read recent messages |
| `egregore_search` | Full-text search |
| `egregore_status` | Get node status |

## Debug

| Hook | Log file |
|------|----------|
| `claude-agent-hook.py` | `/tmp/egregore-hook.log` |
| `claude-disciplined-hook.py` | `/tmp/egregore-disciplined-hook.log` |

## Files

```
claude-hook/
├── claude-agent-hook.py       # Basic hook
├── claude-disciplined-hook.py # Disciplined hook (recommended)
├── config.yaml.example        # Egregore config
└── README.md
```
