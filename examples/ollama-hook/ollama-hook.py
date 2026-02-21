#!/home/pknull/Code/egregore/.venv/bin/python3
"""
Egregore hook using local Ollama.

Reads message JSON from stdin, invokes a local LLM to generate a response,
then publishes via egregore HTTP API.

Requirements:
    pip install ollama httpx

Environment:
    EGREGORE_API   - Optional (default: http://localhost:7654)
    OLLAMA_MODEL   - Optional (default: qwen2.5-coder:7b)
    OLLAMA_HOST    - Optional (default: http://localhost:11434)
"""

import json
import logging
import os
import sys
from datetime import datetime

import httpx
import ollama

# Configuration
EGREGORE_API = os.environ.get("EGREGORE_API", "http://localhost:7654")
OLLAMA_MODEL = os.environ.get("OLLAMA_MODEL", "qwen2.5-coder:7b")
OLLAMA_HOST = os.environ.get("OLLAMA_HOST", "http://localhost:11434")
MAX_BODY_SIZE = 8192
LOG_FILE = "/tmp/egregore-ollama-hook.log"

# Setup file logging
logging.basicConfig(
    filename=LOG_FILE,
    level=logging.DEBUG,
    format='%(asctime)s %(levelname)s: %(message)s'
)
log = logging.getLogger(__name__)

# HTTP client for egregore API calls
http = httpx.Client(base_url=EGREGORE_API, timeout=30.0)

# Configure Ollama client
ollama_client = ollama.Client(host=OLLAMA_HOST)


def get_self_identity() -> str:
    """Fetch this node's public_id."""
    try:
        resp = http.get("/v1/identity")
        resp.raise_for_status()
        return resp.json().get("data", {}).get("public_id", "")
    except Exception as e:
        log.warning("Could not fetch identity: %s", e)
        return ""


def publish_response(body: str, in_reply_to: str) -> bool:
    """Publish a response to egregore."""
    try:
        payload = {
            "content": {
                "type": "response",
                "body": body,
                "in_reply_to": in_reply_to,
            },
        }
        resp = http.post("/v1/publish", json=payload)
        resp.raise_for_status()
        result = resp.json()
        log.info("Published response: %s", result.get("data", {}).get("hash", "")[:16])
        return True
    except Exception as e:
        log.error("Failed to publish: %s", e)
        return False


def generate_response(prompt: str) -> str:
    """Generate a response using Ollama."""
    try:
        log.info("Calling Ollama model: %s", OLLAMA_MODEL)
        response = ollama_client.chat(
            model=OLLAMA_MODEL,
            messages=[{"role": "user", "content": prompt}],
        )
        result = response.message.content.strip()
        log.debug("Ollama response: %s", result[:200])
        return result
    except Exception as e:
        log.error("Ollama error: %s", e)
        return ""


def main():
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
    prompt = f"""You received a message on a peer-to-peer mesh network.

FROM: {author}
MESSAGE: {body}

Write a brief, helpful response. Just output the response text, nothing else.
Be concise. Do not explain what you're doing."""

    # Generate response
    response = generate_response(prompt)

    if not response:
        log.warning("Ollama produced no output")
        sys.exit(1)

    # Publish the response
    if publish_response(response, msg_hash):
        log.info("Hook completed successfully")
    else:
        log.error("Failed to publish response")
        sys.exit(1)


if __name__ == "__main__":
    main()
