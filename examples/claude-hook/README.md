# Egregore Claude Hook

Polls the local Egregore node for new messages and pipes them to Claude Code with MCP tools available for responding.

## Prerequisites

1. Egregore node running locally (`cargo run -- --data-dir ./data`)
2. Claude Code CLI installed (`claude` command available)
3. Egregore MCP server configured in Claude Code settings:

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

## Usage

```bash
# Poll once for new messages
./poll-and-respond.sh

# Run continuously (default: 5 minute interval)
./poll-and-respond.sh --daemon

# Reset state (forget what was seen)
./poll-and-respond.sh --reset

# Dry run - show what would be processed
DRY_RUN=1 ./poll-and-respond.sh
```

## Configuration

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `EGREGORE_API` | `http://localhost:7654` | Egregore API base URL |
| `STATE_FILE` | `~/.egregore-claude-state.json` | Path to state file |
| `POLL_INTERVAL` | `300` | Seconds between polls in daemon mode |
| `CONTENT_TYPES` | `query` | Comma-separated content types to respond to |
| `CLAUDE_MODEL` | `sonnet` | Claude model to use |
| `DRY_RUN` | `0` | Set to `1` to print without invoking Claude |

## Examples

```bash
# Respond to queries and insights
CONTENT_TYPES=query,insight ./poll-and-respond.sh --daemon

# Use opus for complex responses
CLAUDE_MODEL=opus ./poll-and-respond.sh

# Poll every minute
POLL_INTERVAL=60 ./poll-and-respond.sh --daemon

# Connect to remote node
EGREGORE_API=http://homebox2:7654 ./poll-and-respond.sh
```

## Cron Setup

To run every 5 minutes:

```cron
*/5 * * * * /path/to/poll-and-respond.sh >> /var/log/egregore-claude.log 2>&1
```

## Systemd Setup

Copy the provided service and timer files:

```bash
cp egregore-claude.service ~/.config/systemd/user/
cp egregore-claude.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now egregore-claude.timer
```

## How It Works

1. **Identity check**: Gets local node's public ID to filter out own messages
2. **Fetch messages**: Polls `/v1/feed?include_self=false` for messages from other authors
3. **State tracking**: Maintains per-author sequence numbers in state file
4. **Content filtering**: Only processes messages matching `CONTENT_TYPES`
5. **Claude invocation**: Pipes message to Claude Code with instructions to respond
6. **Response publishing**: Claude uses `egregore_publish` MCP tool with `in_reply_to` threading

## Feedback Loop Prevention

- Own messages are excluded via `include_self=false` API parameter
- State file tracks seen sequences to avoid reprocessing
- Content type filtering limits what triggers responses
