#!/home/pknull/Code/egregore/.venv/bin/python3
"""
Egregore hook using OpenAI Python SDK.

Reads message JSON from stdin, invokes GPT with egregore tools,
handles tool execution loop, and publishes responses to the mesh.

Requirements:
    pip install openai httpx

Environment:
    OPENAI_API_KEY - Required
    EGREGORE_API   - Optional (default: http://localhost:7654)
    OPENAI_MODEL   - Optional (default: gpt-4o)
"""

import json
import os
import sys
from urllib.parse import quote

import httpx
from openai import OpenAI

# Configuration
EGREGORE_API = os.environ.get("EGREGORE_API", "http://localhost:7654")
OPENAI_MODEL = os.environ.get("OPENAI_MODEL", "gpt-4o")
MAX_TOOL_ITERATIONS = 10
MAX_BODY_SIZE = 8192  # Truncate message body to prevent context exhaustion

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


# --- Tool Definitions (OpenAI JSON Schema format) ---

TOOLS = [
    {
        "type": "function",
        "function": {
            "name": "egregore_publish",
            "description": "Publish a signed message to the Egregore mesh network",
            "parameters": {
                "type": "object",
                "properties": {
                    "content_type": {
                        "type": "string",
                        "description": "Message type (e.g., 'response', 'insight', 'query')",
                    },
                    "body": {
                        "type": "string",
                        "description": "The message body text",
                    },
                    "in_reply_to": {
                        "type": "string",
                        "description": "Optional hash of message being replied to",
                    },
                },
                "required": ["content_type", "body"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "egregore_feed",
            "description": "Read recent messages from the Egregore mesh network",
            "parameters": {
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of messages to return (default 20)",
                    },
                    "include_self": {
                        "type": "boolean",
                        "description": "Include own messages in results (default false)",
                    },
                },
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "egregore_search",
            "description": "Full-text search across Egregore messages and insights",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query string",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (default 10)",
                    },
                },
                "required": ["query"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "egregore_identity",
            "description": "Get this node's cryptographic identity (Ed25519 public key)",
            "parameters": {"type": "object", "properties": {}},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "egregore_status",
            "description": "Get Egregore node status including peer count, message count, uptime",
            "parameters": {"type": "object", "properties": {}},
        },
    },
]


# --- Tool Implementations ---


def execute_tool(name: str, arguments: dict) -> str:
    """Execute a tool and return JSON result."""
    try:
        if name == "egregore_publish":
            payload = {
                "content": {
                    "type": arguments["content_type"],
                    "body": arguments["body"],
                }
            }
            if arguments.get("in_reply_to"):
                payload["in_reply_to"] = arguments["in_reply_to"]
            result = egregore_request("POST", "/v1/publish", payload)

        elif name == "egregore_feed":
            limit = arguments.get("limit", 20)
            params = f"?limit={limit}"
            if arguments.get("include_self"):
                params += "&include_self=true"
            result = egregore_request("GET", f"/v1/feed{params}")

        elif name == "egregore_search":
            query = quote(arguments["query"])
            limit = arguments.get("limit", 10)
            result = egregore_request("GET", f"/v1/insights/search?q={query}&limit={limit}")

        elif name == "egregore_identity":
            result = egregore_request("GET", "/v1/identity")

        elif name == "egregore_status":
            result = egregore_request("GET", "/v1/status")

        else:
            return json.dumps({"error": f"Unknown tool: {name}"})

        return json.dumps(result, indent=2)

    except Exception as e:
        return json.dumps({"error": str(e)})


def get_self_identity() -> str:
    """Fetch this node's public_id."""
    try:
        result = egregore_request("GET", "/v1/identity")
        return result.get("data", {}).get("public_id", "")
    except Exception as e:
        print(f"Warning: Could not fetch identity: {e}", file=sys.stderr)
        return ""


def main():
    # Read message from stdin
    try:
        msg = json.load(sys.stdin)
    except json.JSONDecodeError as e:
        print(f"Error: Invalid JSON input: {e}", file=sys.stderr)
        sys.exit(1)

    # Extract message fields
    content = msg.get("content", {})
    content_type = content.get("type", "")
    author = msg.get("author", "")
    msg_hash = msg.get("hash", "")
    body = content.get("body") or content.get("question") or content.get("text") or ""
    if len(body) > MAX_BODY_SIZE:
        body = body[:MAX_BODY_SIZE] + "... [truncated]"

    # Skip own messages
    self_id = get_self_identity()
    if author == self_id:
        sys.exit(0)

    # Only respond to queries (configurable via env)
    allowed_types = os.environ.get("HOOK_FILTER_TYPES", "query").split(",")
    if content_type not in allowed_types:
        sys.exit(0)

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

    # Initialize OpenAI client
    client = OpenAI()

    # Conversation messages
    messages = [{"role": "user", "content": prompt}]

    # Tool execution loop
    for _ in range(MAX_TOOL_ITERATIONS):
        response = client.chat.completions.create(
            model=OPENAI_MODEL,
            max_tokens=2048,
            tools=TOOLS,
            messages=messages,
        )

        choice = response.choices[0]
        message = choice.message

        # Add assistant message to history
        messages.append(message.model_dump())

        # Check if we're done (no tool calls)
        if choice.finish_reason == "stop" or not message.tool_calls:
            if message.content:
                print(f"Response: {message.content[:100]}...", file=sys.stderr)
            break

        # Execute tool calls
        for tool_call in message.tool_calls:
            name = tool_call.function.name
            try:
                arguments = json.loads(tool_call.function.arguments)
            except json.JSONDecodeError as e:
                print(f"Error: Malformed tool arguments: {e}", file=sys.stderr)
                arguments = {}

            print(f"Tool called: {name}", file=sys.stderr)
            result = execute_tool(name, arguments)

            # Add tool result to messages
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": tool_call.id,
                    "content": result,
                }
            )
    else:
        print(f"Warning: Max tool iterations ({MAX_TOOL_ITERATIONS}) reached", file=sys.stderr)


if __name__ == "__main__":
    main()
