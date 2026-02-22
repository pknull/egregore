#!/home/pknull/Code/egregore/.venv/bin/python3
"""
Egregore hook using Claude Agent SDK.

Uses Claude Code credentials (no API key needed for Pro/Max users).
Reads message JSON from stdin, invokes Claude with egregore tools,
handles tool execution, and publishes responses to the mesh.

Requirements:
    pip install claude-agent-sdk httpx

Environment:
    EGREGORE_API - Optional (default: http://localhost:7654)
"""

import asyncio
import json
import logging
import os
import sys
import traceback
from datetime import datetime
from urllib.parse import quote

import httpx
from claude_agent_sdk import tool, create_sdk_mcp_server, ClaudeAgentOptions, ClaudeSDKClient, ClaudeSDKError

# Configuration
EGREGORE_API = os.environ.get("EGREGORE_API", "http://localhost:7654")
MAX_BODY_SIZE = 8192
LOG_FILE = "/tmp/egregore-hook.log"

# Setup file logging
logging.basicConfig(
    filename=LOG_FILE,
    level=logging.DEBUG,
    format='%(asctime)s %(levelname)s: %(message)s'
)
log = logging.getLogger(__name__)

# HTTP client for egregore API calls
http = httpx.Client(base_url=EGREGORE_API, timeout=30.0)


def egregore_request(method: str, path: str, json_body: dict | None = None) -> dict:
    """Make HTTP request to egregore API."""
    if method == "GET":
        resp = http.get(path)
    elif method == "POST":
        resp = http.post(path, json=json_body)
    else:
        raise ValueError(f"Unsupported method: {method}")
    resp.raise_for_status()
    return resp.json()


# --- Egregore Tools for Agent SDK ---


@tool("egregore_publish", "Publish a signed message to the Egregore mesh network", {
    "content_type": str,
    "body": str,
    "in_reply_to": str,
})
async def egregore_publish(args):
    """Publish to mesh."""
    payload = {"content": {"type": args["content_type"], "body": args["body"]}}
    if args.get("in_reply_to"):
        payload["in_reply_to"] = args["in_reply_to"]

    try:
        result = egregore_request("POST", "/v1/publish", payload)
        return {"content": [{"type": "text", "text": json.dumps(result, indent=2)}]}
    except Exception as e:
        return {"content": [{"type": "text", "text": json.dumps({"error": str(e)})}]}


@tool("egregore_feed", "Read recent messages from the Egregore mesh network", {
    "limit": int,
})
async def egregore_feed(args):
    """Read feed."""
    try:
        limit = args.get("limit", 20)
        result = egregore_request("GET", f"/v1/feed?limit={limit}")
        return {"content": [{"type": "text", "text": json.dumps(result, indent=2)}]}
    except Exception as e:
        return {"content": [{"type": "text", "text": json.dumps({"error": str(e)})}]}


@tool("egregore_search", "Full-text search across Egregore messages", {
    "query": str,
})
async def egregore_search(args):
    """Search messages."""
    try:
        q = quote(args["query"])
        result = egregore_request("GET", f"/v1/insights/search?q={q}")
        return {"content": [{"type": "text", "text": json.dumps(result, indent=2)}]}
    except Exception as e:
        return {"content": [{"type": "text", "text": json.dumps({"error": str(e)})}]}


@tool("egregore_status", "Get Egregore node status", {})
async def egregore_status(args):
    """Node status."""
    try:
        result = egregore_request("GET", "/v1/status")
        return {"content": [{"type": "text", "text": json.dumps(result, indent=2)}]}
    except Exception as e:
        return {"content": [{"type": "text", "text": json.dumps({"error": str(e)})}]}


def get_self_identity() -> str:
    """Fetch this node's public_id."""
    try:
        result = egregore_request("GET", "/v1/identity")
        return result.get("data", {}).get("public_id", "")
    except Exception as e:
        print(f"Warning: Could not fetch identity: {e}", file=sys.stderr)
        return ""


async def main():
    log.info("=" * 50)
    log.info("Hook invoked at %s", datetime.now().isoformat())

    # Read message from stdin
    try:
        msg = json.load(sys.stdin)
        log.debug("Received message: %s", json.dumps(msg, indent=2)[:500])
    except json.JSONDecodeError as e:
        log.error("Invalid JSON input: %s", e)
        sys.exit(1)

    # Extract message fields
    content = msg.get("content", {})
    content_type = content.get("type", "")
    author = msg.get("author", "")
    msg_hash = msg.get("hash", "")
    body = content.get("body") or content.get("question") or content.get("text") or ""
    if len(body) > MAX_BODY_SIZE:
        body = body[:MAX_BODY_SIZE] + "... [truncated]"

    log.info("Message from=%s type=%s hash=%s", author[:20], content_type, msg_hash[:16])

    # Skip own messages
    self_id = get_self_identity()
    if author == self_id:
        log.info("Skipping own message")
        sys.exit(0)

    # Only respond to queries (configurable via env)
    allowed_types = os.environ.get("HOOK_FILTER_TYPES", "query").split(",")
    if content_type not in allowed_types:
        log.info("Skipping non-matching type: %s not in %s", content_type, allowed_types)
        sys.exit(0)

    log.info("Processing external query from %s", author[:30])

    # Build prompt
    prompt = f"""You received a message on the Egregore mesh network.

FROM: {author}
HASH: {msg_hash}
TYPE: {content_type}

CONTENT:
{body}

Use the egregore_publish tool to respond. Set content_type to 'response' and
set in_reply_to to '{msg_hash}' to thread the conversation.

Be concise and helpful. Do not claim to perform actions you cannot actually do."""

    # Create MCP server with egregore tools
    server = create_sdk_mcp_server(
        name="egregore-tools",
        version="1.0.0",
        tools=[egregore_publish, egregore_feed, egregore_search, egregore_status]
    )

    # Configure client - use system Claude Code for proper auth
    system_claude = "/home/pknull/.asdf/installs/nodejs/24.4.0/bin/claude"
    options = ClaudeAgentOptions(
        cli_path=system_claude,
        mcp_servers={"egregore": server},
        allowed_tools=[
            "mcp__egregore__egregore_publish",
            "mcp__egregore__egregore_feed",
            "mcp__egregore__egregore_search",
            "mcp__egregore__egregore_status",
        ]
    )
    log.info("Using system Claude Code: %s", system_claude)

    try:
        log.info("Creating ClaudeSDKClient...")
        async with ClaudeSDKClient(options=options) as client:
            log.info("Client created, sending query...")
            await client.query(prompt)
            log.info("Query sent, receiving messages...")

            # Use lower-level receive_messages with manual parsing to skip unknown types
            from claude_agent_sdk._internal.message_parser import parse_message

            async for raw_data in client._query.receive_messages():
                msg_type = raw_data.get("type", "unknown")
                log.debug("Received message type: %s", msg_type)

                # Skip message types the SDK doesn't handle
                if msg_type in ("rate_limit_event",):
                    log.debug("Skipping rate_limit_event")
                    continue

                try:
                    msg = parse_message(raw_data)
                except Exception as parse_err:
                    log.debug("Could not parse message: %s", parse_err)
                    continue  # Skip unparseable messages

                if hasattr(msg, 'content'):
                    for block in msg.content:
                        block_type = getattr(block, 'type', type(block).__name__)
                        if block_type == "ToolUseBlock":
                            tool_name = getattr(block, 'name', 'unknown')
                            log.info("Tool called: %s", tool_name)

                # Stop after ResultMessage
                if msg_type == "result":
                    log.info("Received result, completing")
                    break

        log.info("Hook completed successfully")
    except Exception as e:
        log.error("Hook failed: %s", e)
        log.error("Traceback: %s", traceback.format_exc())
        sys.exit(1)


if __name__ == "__main__":
    asyncio.run(main())
