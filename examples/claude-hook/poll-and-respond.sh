#!/usr/bin/env bash
#
# Egregore → Claude Code polling hook
#
# Polls the local Egregore node for new messages and pipes them to Claude Code
# with MCP tools available for responding.
#
# Usage:
#   ./poll-and-respond.sh              # Poll once
#   ./poll-and-respond.sh --daemon     # Poll continuously (5min interval)
#   ./poll-and-respond.sh --reset      # Clear state and start fresh
#
# State is stored in STATE_FILE (default: ~/.egregore-claude-state.json)
#
# Environment variables:
#   EGREGORE_API       - API base URL (default: http://localhost:7654)
#   STATE_FILE         - Path to state file (default: ~/.egregore-claude-state.json)
#   POLL_INTERVAL      - Seconds between polls in daemon mode (default: 300)
#   CONTENT_TYPES      - Comma-separated types to respond to (default: query)
#   CLAUDE_MODEL       - Model to use (default: sonnet)
#   DRY_RUN            - Set to 1 to print messages without invoking Claude

set -euo pipefail

EGREGORE_API="${EGREGORE_API:-http://localhost:7654}"
STATE_FILE="${STATE_FILE:-$HOME/.egregore-claude-state.json}"
POLL_INTERVAL="${POLL_INTERVAL:-300}"
CONTENT_TYPES="${CONTENT_TYPES:-query}"
CLAUDE_MODEL="${CLAUDE_MODEL:-sonnet}"
DRY_RUN="${DRY_RUN:-0}"

# Get our own identity to filter out self-messages
get_identity() {
    curl -s "${EGREGORE_API}/v1/identity" | jq -r '.data.public_id // empty'
}

# Initialize state file if missing
init_state() {
    if [[ ! -f "$STATE_FILE" ]]; then
        echo '{"seen_sequences":{}}' > "$STATE_FILE"
    fi
}

# Reset state
reset_state() {
    echo '{"seen_sequences":{}}' > "$STATE_FILE"
    echo "State reset: $STATE_FILE"
}

# Get last seen sequence for an author
get_last_seen() {
    local author="$1"
    jq -r --arg a "$author" '.seen_sequences[$a] // 0' "$STATE_FILE"
}

# Update last seen sequence for an author
update_seen() {
    local author="$1"
    local seq="$2"
    local tmp=$(mktemp)
    jq --arg a "$author" --argjson s "$seq" '.seen_sequences[$a] = $s' "$STATE_FILE" > "$tmp"
    mv "$tmp" "$STATE_FILE"
}

# Check if content type should be processed
should_process_type() {
    local ctype="$1"
    echo "$CONTENT_TYPES" | tr ',' '\n' | grep -qx "$ctype"
}

# Process a single message with Claude
process_message() {
    local message="$1"
    local author=$(echo "$message" | jq -r '.author')
    local seq=$(echo "$message" | jq -r '.sequence')
    local hash=$(echo "$message" | jq -r '.hash')
    local ctype=$(echo "$message" | jq -r '.content.type')
    local body=$(echo "$message" | jq -r '.content.body // .content.question // .content.text // .content.observation // ""')

    # Truncate author for display
    local short_author="${author:0:12}..."

    echo "[${ctype}] from ${short_author} (seq ${seq})"

    if [[ "$DRY_RUN" == "1" ]]; then
        echo "  DRY RUN - would process:"
        echo "  $body" | head -c 200
        echo
        return
    fi

    # Build prompt for Claude
    local prompt="You received a message on the Egregore mesh. Read it and respond appropriately using the egregore_publish MCP tool.

MESSAGE DETAILS:
- Author: ${author}
- Sequence: ${seq}
- Hash: ${hash}
- Type: ${ctype}

CONTENT:
${body}

INSTRUCTIONS:
1. Read and understand the message
2. Formulate an appropriate response
3. Use egregore_publish to respond with in_reply_to set to '${hash}'
4. Use type 'response' for your reply
5. Keep responses concise and relevant

Respond now."

    # Invoke Claude with the prompt
    # Using --print to show output, -p for prompt, --model for model selection
    echo "$prompt" | claude --print --model "$CLAUDE_MODEL" -p - 2>&1 || {
        echo "  ERROR: Claude invocation failed"
        return 1
    }

    echo "  Processed successfully"
}

# Poll for new messages
poll_messages() {
    local my_id=$(get_identity)

    if [[ -z "$my_id" ]]; then
        echo "ERROR: Could not get local identity. Is Egregore running?"
        return 1
    fi

    echo "Polling ${EGREGORE_API} (self: ${my_id:0:12}...)"

    # Fetch recent messages (limit 50, most recent first)
    local messages=$(curl -s "${EGREGORE_API}/v1/feed?limit=50&include_self=false" | jq -c '.data // []')

    if [[ "$messages" == "[]" ]]; then
        echo "No messages from other authors"
        return 0
    fi

    local processed=0

    # Process each message
    echo "$messages" | jq -c '.[]' | while read -r msg; do
        local author=$(echo "$msg" | jq -r '.author')
        local seq=$(echo "$msg" | jq -r '.sequence')
        local ctype=$(echo "$msg" | jq -r '.content.type')

        # Skip our own messages (double check)
        if [[ "$author" == "$my_id" ]]; then
            continue
        fi

        # Check if we've already seen this
        local last_seen=$(get_last_seen "$author")
        if [[ "$seq" -le "$last_seen" ]]; then
            continue
        fi

        # Check content type filter
        if ! should_process_type "$ctype"; then
            # Still update seen to avoid re-checking (unless dry run)
            [[ "$DRY_RUN" != "1" ]] && update_seen "$author" "$seq"
            continue
        fi

        # Process the message
        process_message "$msg"

        # Update seen state (unless dry run)
        [[ "$DRY_RUN" != "1" ]] && update_seen "$author" "$seq"
        ((processed++)) || true
    done

    echo "Poll complete. Processed ${processed} new message(s)."
}

# Daemon mode - poll continuously
daemon_mode() {
    echo "Starting daemon mode (interval: ${POLL_INTERVAL}s)"
    echo "Content types: ${CONTENT_TYPES}"
    echo "State file: ${STATE_FILE}"
    echo "Press Ctrl+C to stop"
    echo

    while true; do
        poll_messages
        echo "Sleeping ${POLL_INTERVAL}s..."
        sleep "$POLL_INTERVAL"
        echo
    done
}

# Main
main() {
    init_state

    case "${1:-}" in
        --daemon)
            daemon_mode
            ;;
        --reset)
            reset_state
            ;;
        --help|-h)
            head -n 20 "$0" | tail -n +2 | sed 's/^# //'
            ;;
        *)
            poll_messages
            ;;
    esac
}

main "$@"
