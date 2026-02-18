#!/usr/bin/env bash
# discord-notify.sh — Forward egregore messages to Discord via webhook.
# Receives message JSON on stdin from the hook executor.
#
# Configuration (in order of precedence):
#   1. DISCORD_WEBHOOK_URL environment variable
#   2. Contents of ~/.egregore/discord-webhook.url

set -euo pipefail

# Load webhook URL
if [[ -z "${DISCORD_WEBHOOK_URL:-}" ]]; then
    URL_FILE="${HOME}/.egregore/discord-webhook.url"
    if [[ -f "$URL_FILE" ]]; then
        DISCORD_WEBHOOK_URL=$(cat "$URL_FILE")
    else
        echo "No DISCORD_WEBHOOK_URL set and $URL_FILE not found" >&2
        exit 1
    fi
fi

# Read message JSON from stdin
MSG=$(cat)

# Extract fields
AUTHOR=$(echo "$MSG" | jq -r '.author // "unknown"')
TIMESTAMP=$(echo "$MSG" | jq -r '.timestamp // empty')
HASH=$(echo "$MSG" | jq -r '.hash // empty')
CONTENT_TYPE=$(echo "$MSG" | jq -r '.content.type // "unknown"')
TEXT=$(echo "$MSG" | jq -r '.content.text // empty')
TOPIC=$(echo "$MSG" | jq -r '.content.topic // empty')
SEQUENCE=$(echo "$MSG" | jq -r '.sequence // 0')

# Truncate author ID for readability: @+K6IB8+...xw0=.ed25519 -> @+K6IB8+...ed25519
SHORT_AUTHOR="${AUTHOR:0:10}...ed25519"

# Build description
DESCRIPTION=""
if [[ -n "$TEXT" ]]; then
    # Truncate long text for Discord (max 4096 for embed description)
    if [[ ${#TEXT} -gt 2000 ]]; then
        TEXT="${TEXT:0:2000}..."
    fi
    DESCRIPTION="$TEXT"
else
    # Fall back to showing raw content JSON (compact)
    DESCRIPTION=$(echo "$MSG" | jq -c '.content')
    if [[ ${#DESCRIPTION} -gt 2000 ]]; then
        DESCRIPTION="${DESCRIPTION:0:2000}..."
    fi
fi

# Build footer parts
FOOTER="seq $SEQUENCE"
if [[ -n "$TOPIC" ]]; then
    FOOTER="$FOOTER | topic: $TOPIC"
fi
if [[ -n "$HASH" ]]; then
    SHORT_HASH="${HASH:0:12}"
    FOOTER="$FOOTER | $SHORT_HASH"
fi

# Color based on content type
case "$CONTENT_TYPE" in
    response) COLOR=5814783 ;;   # blue
    query)    COLOR=16776960 ;;  # yellow
    insight)  COLOR=5763719 ;;   # green
    *)        COLOR=9807270 ;;   # grey
esac

# Build Discord payload
PAYLOAD=$(jq -n \
    --arg username "Egregore" \
    --arg title "$CONTENT_TYPE from $SHORT_AUTHOR" \
    --arg description "$DESCRIPTION" \
    --arg footer "$FOOTER" \
    --arg timestamp "$TIMESTAMP" \
    --argjson color "$COLOR" \
    '{
        username: $username,
        embeds: [{
            title: $title,
            description: $description,
            color: $color,
            footer: { text: $footer },
            timestamp: $timestamp
        }]
    }')

# Post to Discord
curl -sf -H "Content-Type: application/json" \
    -d "$PAYLOAD" \
    "$DISCORD_WEBHOOK_URL" > /dev/null
