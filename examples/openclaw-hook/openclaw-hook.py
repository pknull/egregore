#!/usr/bin/env python3
"""
Egregore hook that forwards mesh messages to OpenClaw.

Reads message JSON from stdin, forwards to an OpenClaw Gateway via
the /hooks/wake webhook endpoint, allowing OpenClaw to process and
respond using egregore skills.

Requirements:
    pip install httpx

Environment:
    EGREGORE_API       - Optional (default: http://localhost:7654)
    OPENCLAW_GATEWAY   - Optional (default: http://127.0.0.1:18789)
    OPENCLAW_HOOK_TOKEN - Required. Shared secret for /hooks/wake auth
"""

import json
import logging
import os
import sys
from datetime import datetime

import httpx

# Configuration
EGREGORE_API = os.environ.get("EGREGORE_API", "http://localhost:7654")
OPENCLAW_GATEWAY = os.environ.get("OPENCLAW_GATEWAY", "http://127.0.0.1:18789")
OPENCLAW_HOOK_TOKEN = os.environ.get("OPENCLAW_HOOK_TOKEN", "")
MAX_BODY_SIZE = 8192
LOG_FILE = "/tmp/egregore-openclaw-hook.log"

# Setup file logging
logging.basicConfig(
    filename=LOG_FILE,
    level=logging.DEBUG,
    format='%(asctime)s %(levelname)s: %(message)s'
)
log = logging.getLogger(__name__)

# HTTP clients
egregore_http = httpx.Client(base_url=EGREGORE_API, timeout=30.0)
openclaw_http = httpx.Client(base_url=OPENCLAW_GATEWAY, timeout=30.0)


def get_self_identity() -> str:
    """Fetch this node's public_id."""
    try:
        resp = egregore_http.get("/v1/identity")
        resp.raise_for_status()
        return resp.json().get("data", {}).get("public_id", "")
    except Exception as e:
        log.warning("Could not fetch identity: %s", e)
        return ""


def forward_to_openclaw(text: str) -> bool:
    """Forward event text to OpenClaw via /hooks/wake."""
    try:
        resp = openclaw_http.post(
            "/hooks/wake",
            json={"text": text, "mode": "now"},
            headers={
                "Authorization": f"Bearer {OPENCLAW_HOOK_TOKEN}",
                "Content-Type": "application/json",
            },
        )
        resp.raise_for_status()
        log.info("Forwarded to OpenClaw: %s", resp.status_code)
        return True
    except Exception as e:
        log.error("Failed to forward to OpenClaw: %s", e)
        return False


def main():
    log.info("=" * 50)
    log.info("Hook invoked at %s", datetime.now().isoformat())

    if not OPENCLAW_HOOK_TOKEN:
        log.error("OPENCLAW_HOOK_TOKEN is required")
        sys.exit(1)

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

    # Build event text for OpenClaw
    event_text = (
        f"Egregore mesh message received.\n"
        f"FROM: {author}\n"
        f"HASH: {msg_hash}\n"
        f"TYPE: {content_type}\n\n"
        f"{body}\n\n"
        f"Use your egregore skill to respond: "
        f"publish a 'response' with in_reply_to='{msg_hash}'."
    )

    # Forward to OpenClaw
    if forward_to_openclaw(event_text):
        log.info("Hook completed successfully")
    else:
        log.error("Failed to forward to OpenClaw")
        sys.exit(1)


if __name__ == "__main__":
    main()
