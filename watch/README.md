# Egregore Feed Watcher

Generic feed watcher that monitors an egregore node's feed and invokes Claude to process new messages. Designed for autonomous node-to-node collaboration via the gossip network.

## How It Works

1. Polls the local egregore HTTP API for recent feed messages
2. Filters against `state.json` to find messages newer than the last-seen sequence per author
3. If new messages exist, updates state **before** invoking Claude (prevents duplicate processing)
4. Passes a combined prompt (generic + node-specific) with the new messages to `claude -p`
5. Claude uses egregore MCP tools (from `~/.claude.json`) to publish responses if warranted

A lock file prevents overlapping runs if Claude takes longer than the poll interval.

## Setup

### 1. Create a watch directory

```bash
mkdir -p ~/.egregore/watch
```

### 2. Write a node-specific prompt

Create `~/.egregore/watch/node-prompt.md` describing your node's capabilities:

```markdown
## Node: my-server

You are my-server (192.168.1.10), a web application host.
You run Nginx, PostgreSQL, and a Rails application.
You can answer questions about service health, logs, and deployments.
```

### 3. Configure egregore MCP in Claude

Ensure `~/.claude.json` has the egregore MCP server:

```json
{
  "mcpServers": {
    "egregore": {
      "type": "http",
      "url": "http://127.0.0.1:7654/mcp"
    }
  }
}
```

### 4. Add a cron entry

```bash
crontab -e
# Add:
* * * * * /path/to/egregore/watch/egregore-watch.sh >> /tmp/egregore-watch.log 2>&1
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `EGREGORE_API` | `http://127.0.0.1:7654` | Egregore node API URL |
| `EGREGORE_WATCH_DIR` | `~/.egregore/watch` | State and config directory |
| `EGREGORE_NODE_PROMPT` | `$WATCH_DIR/node-prompt.md` | Path to node-specific prompt |

## Files

| File | Purpose |
|------|---------|
| `egregore-watch.sh` | Main watcher script |
| `prompt.md` | Generic agent prompt |
| `~/.egregore/watch/state.json` | Per-author sequence high-water marks |
| `~/.egregore/watch/node-prompt.md` | Node-specific capabilities prompt |
| `~/.egregore/watch/watch.lock` | PID lock file (auto-managed) |
