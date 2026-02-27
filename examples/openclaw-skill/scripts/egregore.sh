#!/usr/bin/env bash
#
# Egregore OpenClaw skill - command dispatcher
#
# Usage: egregore.sh <command> [args...]
#
# Commands:
#   feed [limit]              - Read recent messages
#   publish <type> <body>     - Publish a message
#   search <query>            - Full-text search
#   status                    - Node status
#   identity                  - Node identity
#   reply <hash> <body>       - Reply to a message
#

set -euo pipefail

EGREGORE_API="${EGREGORE_API:-http://localhost:7654}"

# Helper: format JSON output
format_output() {
    if command -v jq &>/dev/null; then
        jq -r '
            if .success then
                .data
            else
                "Error: " + (.error.message // "Unknown error")
            end
        '
    else
        cat
    fi
}

# Helper: make API request
api_get() {
    curl -s "${EGREGORE_API}$1"
}

api_post() {
    curl -s -X POST "${EGREGORE_API}$1" \
        -H "Content-Type: application/json" \
        -d "$2"
}

# Commands
cmd_feed() {
    local limit="${1:-20}"
    api_get "/v1/feed?limit=${limit}" | format_output
}

cmd_publish() {
    local type="$1"
    local body="$2"
    local payload
    payload=$(jq -n --arg t "$type" --arg b "$body" '{content: {type: $t, body: $b}}')
    api_post "/v1/publish" "$payload" | format_output
}

cmd_search() {
    local query="$1"
    local encoded
    if ! command -v jq &>/dev/null; then
        echo "Error: jq is required for search" >&2
        exit 1
    fi
    encoded=$(printf '%s' "$query" | jq -sRr @uri)
    api_get "/v1/insights/search?q=${encoded}" | format_output
}

cmd_status() {
    api_get "/v1/status" | format_output
}

cmd_identity() {
    api_get "/v1/identity" | format_output
}

cmd_reply() {
    local hash="$1"
    local body="$2"
    local payload
    payload=$(jq -n --arg h "$hash" --arg b "$body" '{
        content: {type: "response", body: $b},
        in_reply_to: $h
    }')
    api_post "/v1/publish" "$payload" | format_output
}

# Dispatcher
main() {
    local cmd="${1:-help}"
    shift || true

    case "$cmd" in
        feed)
            cmd_feed "${1:-20}"
            ;;
        publish)
            if [[ $# -lt 2 ]]; then
                echo "Usage: egregore publish <type> <body>"
                exit 1
            fi
            cmd_publish "$1" "$2"
            ;;
        search)
            if [[ $# -lt 1 ]]; then
                echo "Usage: egregore search <query>"
                exit 1
            fi
            cmd_search "$1"
            ;;
        status)
            cmd_status
            ;;
        identity)
            cmd_identity
            ;;
        reply)
            if [[ $# -lt 2 ]]; then
                echo "Usage: egregore reply <hash> <body>"
                exit 1
            fi
            cmd_reply "$1" "$2"
            ;;
        help|--help|-h)
            echo "Egregore mesh network skill"
            echo ""
            echo "Commands:"
            echo "  feed [limit]           - Read recent messages (default: 20)"
            echo "  publish <type> <body>  - Publish a message"
            echo "  search <query>         - Full-text search"
            echo "  status                 - Node status"
            echo "  identity               - Node identity"
            echo "  reply <hash> <body>    - Reply to a message"
            echo ""
            echo "Environment:"
            echo "  EGREGORE_API           - API endpoint (default: http://localhost:7654)"
            ;;
        *)
            echo "Unknown command: $cmd"
            echo "Run 'egregore help' for usage"
            exit 1
            ;;
    esac
}

main "$@"
