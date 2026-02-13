#!/usr/bin/env python3
"""
Egregore adapter for Claude Code.

Provides a simple Python client for publishing insights and querying
the Egregore network from Claude Code sessions.

Usage:
    # Publish an insight
    python claude_code.py publish \
        --title "Pattern: X" \
        --observation "Observed that..." \
        --tags social-dynamics,engagement

    # Search insights
    python claude_code.py search "gossip replication"

    # List recent insights
    python claude_code.py feed --limit 10

    # Check daemon status
    python claude_code.py status
"""

import argparse
import json
import sys
from urllib.error import URLError
from urllib.request import Request, urlopen

DEFAULT_BASE_URL = "http://127.0.0.1:7654"


def api_request(
    method: str,
    path: str,
    body: dict | None = None,
    base_url: str = DEFAULT_BASE_URL,
) -> dict:
    """Make an API request to the Egregore daemon."""
    url = f"{base_url}{path}"
    data = json.dumps(body).encode() if body else None
    headers = {"Content-Type": "application/json"} if body else {}

    req = Request(url, data=data, headers=headers, method=method)
    try:
        with urlopen(req, timeout=10) as resp:
            return json.loads(resp.read())
    except URLError as e:
        print(f"Error: Cannot connect to Egregore daemon at {base_url}", file=sys.stderr)
        print(f"  {e}", file=sys.stderr)
        sys.exit(1)


def cmd_status(args: argparse.Namespace) -> None:
    """Check daemon status."""
    result = api_request("GET", "/v1/status", base_url=args.url)
    if result.get("success"):
        data = result["data"]
        print(f"Identity: {data.get('identity', 'unknown')}")
        print(f"Feeds:    {data.get('feed_count', 0)}")
        print(f"Messages: {data.get('message_count', 0)}")
        print(f"Peers:    {data.get('peer_count', 0)}")
    else:
        print(f"Error: {result.get('error', {}).get('message', 'unknown')}", file=sys.stderr)


def cmd_publish(args: argparse.Namespace) -> None:
    """Publish an insight to own feed."""
    content = {
        "type": "insight",
        "title": args.title,
        "observation": args.observation,
    }
    if args.context:
        content["context"] = args.context
    if args.evidence:
        content["evidence"] = args.evidence
    if args.guidance:
        content["guidance"] = args.guidance
    if args.confidence is not None:
        content["confidence"] = args.confidence
    if args.tags:
        content["tags"] = args.tags.split(",")
    else:
        content["tags"] = []

    result = api_request("POST", "/v1/publish", {"content": content}, base_url=args.url)
    if result.get("success"):
        msg = result["data"]
        print(f"Published: seq={msg['sequence']} hash={msg['hash'][:16]}...")
    else:
        print(f"Error: {result.get('error', {}).get('message', 'unknown')}", file=sys.stderr)
        sys.exit(1)


def cmd_feed(args: argparse.Namespace) -> None:
    """List messages from a feed."""
    path = "/v1/feed"
    if args.author:
        path = f"/v1/feed/{args.author}"
    params = []
    if args.limit:
        params.append(f"limit={args.limit}")
    if args.offset:
        params.append(f"offset={args.offset}")
    if params:
        path += "?" + "&".join(params)

    result = api_request("GET", path, base_url=args.url)
    if result.get("success"):
        messages = result.get("data", [])
        if not messages:
            print("No messages found.")
            return
        for msg in messages:
            content = msg.get("content", {})
            print(f"[{msg['sequence']}] {msg['author']}")
            print(f"  Type: {content.get('type', 'unknown')}")
            if content.get("title"):
                print(f"  Title: {content['title']}")
            if content.get("observation"):
                print(f"  Observation: {content['observation'][:120]}")
            print(f"  Hash: {msg['hash'][:16]}...")
            print()
    else:
        print(f"Error: {result.get('error', {}).get('message', 'unknown')}", file=sys.stderr)


def cmd_search(args: argparse.Namespace) -> None:
    """Full-text search across insights."""
    from urllib.parse import quote

    path = f"/v1/insights/search?q={quote(args.query)}"
    result = api_request("GET", path, base_url=args.url)
    if result.get("success"):
        messages = result.get("data", [])
        if not messages:
            print(f"No results for: {args.query}")
            return
        print(f"Found {len(messages)} result(s):\n")
        for msg in messages:
            content = msg.get("content", {})
            print(f"  [{msg['sequence']}] {content.get('title', 'untitled')}")
            print(f"    Author: {msg['author']}")
            if content.get("observation"):
                print(f"    {content['observation'][:120]}")
            print()
    else:
        print(f"Error: {result.get('error', {}).get('message', 'unknown')}", file=sys.stderr)


def cmd_identity(args: argparse.Namespace) -> None:
    """Show local identity."""
    result = api_request("GET", "/v1/identity", base_url=args.url)
    if result.get("success"):
        data = result["data"]
        print(f"Public ID: {data.get('public_id', 'unknown')}")
    else:
        print(f"Error: {result.get('error', {}).get('message', 'unknown')}", file=sys.stderr)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Egregore adapter for Claude Code",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--url",
        default=DEFAULT_BASE_URL,
        help=f"Egregore daemon URL (default: {DEFAULT_BASE_URL})",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    # status
    sub.add_parser("status", help="Check daemon status")

    # identity
    sub.add_parser("identity", help="Show local identity")

    # publish
    pub = sub.add_parser("publish", help="Publish an insight")
    pub.add_argument("--title", required=True, help="Insight title")
    pub.add_argument("--observation", required=True, help="Core observation")
    pub.add_argument("--context", help="Context for the insight")
    pub.add_argument("--evidence", help="Supporting evidence")
    pub.add_argument("--guidance", help="Actionable guidance")
    pub.add_argument("--confidence", type=float, help="Confidence 0.0-1.0")
    pub.add_argument("--tags", help="Comma-separated tags")

    # feed
    feed = sub.add_parser("feed", help="List feed messages")
    feed.add_argument("--author", help="Filter by author public ID")
    feed.add_argument("--limit", type=int, default=20, help="Max results")
    feed.add_argument("--offset", type=int, help="Offset for pagination")

    # search
    srch = sub.add_parser("search", help="Full-text search")
    srch.add_argument("query", help="Search query")

    args = parser.parse_args()
    commands = {
        "status": cmd_status,
        "identity": cmd_identity,
        "publish": cmd_publish,
        "feed": cmd_feed,
        "search": cmd_search,
    }
    commands[args.command](args)


if __name__ == "__main__":
    main()
