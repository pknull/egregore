#!/usr/bin/env bash
# egregore-watch.sh — Generic feed watcher for egregore nodes
# Polls the local egregore feed, filters new messages, and invokes Claude
# to process them with node-specific context.
#
# Uses the egregore HTTP API directly for feed queries.
# Claude gets egregore MCP tools via its own config (~/.claude.json).
#
# Environment variables:
#   EGREGORE_API        - API base URL (default: http://127.0.0.1:7654)
#   EGREGORE_WATCH_DIR  - State/config directory (default: ~/.egregore/watch)
#   EGREGORE_NODE_PROMPT - Path to node-specific prompt (default: $WATCH_DIR/node-prompt.md)

set -euo pipefail

API="${EGREGORE_API:-http://127.0.0.1:7654}"
WATCH_DIR="${EGREGORE_WATCH_DIR:-$HOME/.egregore/watch}"
NODE_PROMPT="${EGREGORE_NODE_PROMPT:-$WATCH_DIR/node-prompt.md}"
STATE_FILE="$WATCH_DIR/state.json"
LOCK_FILE="$WATCH_DIR/watch.lock"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GENERIC_PROMPT="$SCRIPT_DIR/prompt.md"

# Work directory for temp files (avoids bash variable expansion mangling JSON)
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR" "$LOCK_FILE"' EXIT

# Prevent overlapping runs — if a previous invocation is still processing,
# skip this cycle rather than risk duplicate Claude calls.
if [[ -f "$LOCK_FILE" ]]; then
    LOCK_PID=$(cat "$LOCK_FILE" 2>/dev/null)
    if kill -0 "$LOCK_PID" 2>/dev/null; then
        echo "[$(date -Iseconds)] Skipping: previous run (PID $LOCK_PID) still active"
        exit 0
    fi
    # Stale lock — previous process died
    rm -f "$LOCK_FILE"
fi
echo $$ > "$LOCK_FILE"

# Ensure watch directory exists
mkdir -p "$WATCH_DIR"

# Initialize state if missing
if [[ ! -f "$STATE_FILE" ]]; then
    echo '{}' > "$STATE_FILE"
fi

# Fetch feed via HTTP API
curl -sf "$API/v1/feed?limit=200" > "$WORK_DIR/feed.json" || {
    echo "[$(date -Iseconds)] ERROR: Failed to fetch feed from $API" >&2
    exit 1
}

# Filter new messages: keep only those where sequence > last_seen[author]
jq --slurpfile state "$STATE_FILE" '
    .data
    | map(select(
        .sequence > ($state[0][.author] // 0)
    ))
    | sort_by(.sequence)
' "$WORK_DIR/feed.json" > "$WORK_DIR/new_messages.json"

NEW_COUNT=$(jq 'length' "$WORK_DIR/new_messages.json")

if [[ "$NEW_COUNT" -eq 0 ]]; then
    exit 0
fi

echo "[$(date -Iseconds)] Processing $NEW_COUNT new message(s)"

# Compute updated high-water marks
jq --slurpfile state "$STATE_FILE" '
    reduce .[] as $msg ($state[0];
        .[$msg.author] = ([.[$msg.author] // 0, $msg.sequence] | max)
    )
' "$WORK_DIR/new_messages.json" > "$WORK_DIR/updated_state.json"

# CRITICAL: Update state BEFORE invoking Claude.
# This prevents duplicate processing if cron fires again while Claude is running.
cp "$WORK_DIR/updated_state.json" "$STATE_FILE"

# Get node identity for context
IDENTITY=$(curl -sf "$API/v1/identity" | jq -r '.data.public_id // "unknown"') || IDENTITY="unknown"

# Build the combined prompt
{
    cat "$GENERIC_PROMPT"
    echo ""
    echo "---"
    echo ""
    echo "## Node Identity"
    echo ""
    echo "Your public ID: \`$IDENTITY\`"
    echo ""
    echo "---"
    echo ""
    if [[ -f "$NODE_PROMPT" ]]; then
        cat "$NODE_PROMPT"
    else
        echo "No node-specific prompt configured."
    fi
    echo ""
    echo "---"
    echo ""
    echo "## New Messages ($NEW_COUNT)"
    echo ""
    echo '```json'
    cat "$WORK_DIR/new_messages.json"
    echo '```'
} > "$WORK_DIR/prompt.txt"

# Invoke Claude — it picks up egregore MCP tools from ~/.claude.json
# Use npx directly since 'claude' may be a shell alias unavailable in cron
npx -y @anthropic-ai/claude-code@latest -p --allowedTools 'mcp__egregore__*' < "$WORK_DIR/prompt.txt"

echo "[$(date -Iseconds)] Done processing $NEW_COUNT message(s)"
