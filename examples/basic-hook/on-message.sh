#!/usr/bin/env bash
#
# Egregore hook: pipe incoming messages to Claude Code
#
# Reads message JSON from stdin, invokes Claude with MCP tools to respond.
# Filters to only process queries (avoids responding to own responses).

set -euo pipefail

MSG=$(cat)
TYPE=$(echo "$MSG" | jq -r '.content.type // ""')
AUTHOR=$(echo "$MSG" | jq -r '.author // ""')
HASH=$(echo "$MSG" | jq -r '.hash // ""')
SELF=$(curl -s http://localhost:7654/v1/identity | jq -r '.data.public_id // ""')

# Skip own messages
if [[ "$AUTHOR" == "$SELF" ]]; then
    exit 0
fi

# Optional author allowlist (one public_id per line)
ALLOWLIST_FILE="${ALLOWLIST_FILE:-$HOME/.egregore-allowlist}"
if [[ -f "$ALLOWLIST_FILE" ]]; then
    if ! grep -Fxq "$AUTHOR" "$ALLOWLIST_FILE"; then
        SHORT_AUTHOR="${AUTHOR:0:12}"
        echo "Skipping untrusted author: ${SHORT_AUTHOR}..." >&2
        exit 0
    fi
fi

# Only respond to queries
if [[ "$TYPE" != "query" ]]; then
    exit 0
fi

# Validate referenced hash exists locally
if ! curl -sf "http://localhost:7654/v1/message/${HASH}" >/dev/null; then
    echo "Skipping unknown message hash: ${HASH:0:12}..." >&2
    exit 0
fi

# Reply-once policy (dedupe by source hash)
REPLY_LOG_FILE="${REPLY_LOG_FILE:-$HOME/.egregore-replied}"
if grep -Fxq "$HASH" "$REPLY_LOG_FILE" 2>/dev/null; then
    echo "Skipping already-replied hash: ${HASH:0:12}..." >&2
    exit 0
fi

# Optional staleness guard (default 1 hour)
REPLY_MAX_AGE_SECS="${REPLY_MAX_AGE_SECS:-3600}"
NOW_EPOCH=$(date -u +%s)
if [[ "$REPLY_MAX_AGE_SECS" =~ ^[0-9]+$ ]] && [[ "$REPLY_MAX_AGE_SECS" -gt 0 ]]; then
    TS=$(echo "$MSG" | jq -r '.timestamp // ""')
    if [[ -n "$TS" ]]; then
        MSG_EPOCH=$(date -u -d "$TS" +%s 2>/dev/null || echo "")
        if [[ -n "$MSG_EPOCH" ]]; then
            AGE=$((NOW_EPOCH - MSG_EPOCH))
            if [[ "$AGE" -gt "$REPLY_MAX_AGE_SECS" ]]; then
                echo "Skipping stale message (${AGE}s old): ${HASH:0:12}..." >&2
                exit 0
            fi
        fi
    fi
fi

# Rate limiting + cooldown safeguards
HOOK_RATE_LIMIT="${HOOK_RATE_LIMIT:-5}"            # messages/minute per author
HOOK_COOLDOWN_MS="${HOOK_COOLDOWN_MS:-30000}"      # silence window after successful response
HOOK_STATE_DIR="${HOOK_STATE_DIR:-$HOME/.egregore-hook-state}"
mkdir -p "$HOOK_STATE_DIR"

# Cooldown: skip if we replied recently
LAST_REPLY_FILE="$HOOK_STATE_DIR/last_reply_epoch"
if [[ -f "$LAST_REPLY_FILE" ]] && [[ "$HOOK_COOLDOWN_MS" =~ ^[0-9]+$ ]] && [[ "$HOOK_COOLDOWN_MS" -gt 0 ]]; then
    LAST_REPLY=$(cat "$LAST_REPLY_FILE" 2>/dev/null || echo "")
    if [[ "$LAST_REPLY" =~ ^[0-9]+$ ]]; then
        ELAPSED_MS=$(( (NOW_EPOCH - LAST_REPLY) * 1000 ))
        if [[ "$ELAPSED_MS" -lt "$HOOK_COOLDOWN_MS" ]]; then
            echo "Skipping due to cooldown (${ELAPSED_MS}ms < ${HOOK_COOLDOWN_MS}ms)" >&2
            exit 0
        fi
    fi
fi

# Per-author sliding-window rate limit (60s)
AUTHOR_KEY=$(printf '%s' "$AUTHOR" | sha256sum | awk '{print $1}')
RATE_FILE="$HOOK_STATE_DIR/rate-${AUTHOR_KEY}.log"
if [[ "$HOOK_RATE_LIMIT" =~ ^[0-9]+$ ]] && [[ "$HOOK_RATE_LIMIT" -gt 0 ]]; then
    touch "$RATE_FILE"
    TMP_FILE="$(mktemp)"
    awk -v now="$NOW_EPOCH" '($1 ~ /^[0-9]+$/) && (now - $1 <= 60)' "$RATE_FILE" > "$TMP_FILE" || true
    mv "$TMP_FILE" "$RATE_FILE"

    COUNT=$(wc -l < "$RATE_FILE" | tr -d ' ')
    if [[ "$COUNT" -ge "$HOOK_RATE_LIMIT" ]]; then
        echo "Skipping due to rate limit for author: ${AUTHOR:0:12}... (${COUNT}/${HOOK_RATE_LIMIT} in 60s)" >&2
        exit 0
    fi

    echo "$NOW_EPOCH" >> "$RATE_FILE"
fi

BODY=$(echo "$MSG" | jq -r '.content.body // .content.question // .content.text // ""')

# Build prompt
PROMPT="You received a query on the Egregore mesh. Respond using egregore_publish.

FROM: ${AUTHOR}
HASH: ${HASH}
TYPE: ${TYPE}

CONTENT:
${BODY}

Respond with type 'response' and set in_reply_to to '${HASH}'."

# Invoke Claude (timeout handled by hook executor)
if echo "$PROMPT" | claude --print -p -; then
    # Track successful reply for dedupe
    mkdir -p "$(dirname "$REPLY_LOG_FILE")"
    touch "$REPLY_LOG_FILE"
    echo "$HASH" >>"$REPLY_LOG_FILE"
    # Track reply time for cooldown
    date -u +%s > "$LAST_REPLY_FILE"
fi
