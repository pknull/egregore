#!/home/pknull/Code/egregore/.venv/bin/python3
"""
Egregore hook using Codex CLI.

Uses Codex credentials (ChatGPT account) - no API key required.
Reads message JSON from stdin, invokes Codex to generate a response,
then publishes via egregore HTTP API.

Requirements:
    - Codex CLI installed: npm install -g @openai/codex
    - Codex authenticated: codex login

Environment:
    EGREGORE_API - Optional (default: http://localhost:7654)
    CODEX_PATH   - Optional path to codex binary
"""

import json
import logging
import os
import subprocess
import sys
from datetime import datetime

import httpx

# Configuration
EGREGORE_API = os.environ.get("EGREGORE_API", "http://localhost:7654")
CODEX_PATH = os.environ.get("CODEX_PATH", "npx")
MAX_BODY_SIZE = 8192
LOG_FILE = "/tmp/egregore-codex-hook.log"

# Setup file logging
logging.basicConfig(
    filename=LOG_FILE,
    level=logging.DEBUG,
    format='%(asctime)s %(levelname)s: %(message)s'
)
log = logging.getLogger(__name__)

# HTTP client for egregore API calls
http = httpx.Client(base_url=EGREGORE_API, timeout=30.0)


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


def run_codex(prompt: str) -> str:
    """Run codex exec with the given prompt."""
    cmd = [
        CODEX_PATH, "-y", "@openai/codex@latest", "exec",
        "--full-auto",
        "--skip-git-repo-check",
        prompt
    ]

    log.info("Running: %s", " ".join(cmd[:4]) + " ...")

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=90,
            cwd=os.path.expanduser("~")
        )

        log.debug("Codex stdout: %s", result.stdout[:500] if result.stdout else "(empty)")
        log.debug("Codex stderr: %s", result.stderr[:200] if result.stderr else "(empty)")

        if result.returncode != 0:
            log.error("Codex failed with code %d: %s", result.returncode, result.stderr[:200])
            return ""

        return result.stdout.strip()

    except subprocess.TimeoutExpired:
        log.error("Codex timed out")
        return ""
    except FileNotFoundError:
        log.error("Codex not found at: %s", CODEX_PATH)
        return ""
    except Exception as e:
        log.error("Codex error: %s", e)
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

    # Build prompt - ask Codex to generate a response (no MCP)
    prompt = f"""You received a message on a peer-to-peer mesh network.

FROM: {author}
MESSAGE: {body}

Write a brief, helpful response. Just output the response text, nothing else.
Be concise. Do not explain what you're doing."""

    # Run Codex
    response = run_codex(prompt)

    if not response:
        log.warning("Codex produced no output")
        sys.exit(1)

    # Publish the response
    if publish_response(response, msg_hash):
        log.info("Hook completed successfully")
    else:
        log.error("Failed to publish response")
        sys.exit(1)


if __name__ == "__main__":
    main()
