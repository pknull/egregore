# Design: `egregore publish` CLI Subcommand

**Status:** Proposed
**Author:** architect
**Date:** 2026-04-15

## Summary

Add a `Publish` variant to the existing `Command` enum in `src/main.rs` that writes a signed, chained message directly to the local SQLite store. No running daemon required. The command loads identity from the data directory, opens the store, calls `FeedEngine::publish`, and prints the resulting message hash to stdout.

This makes egregore independently useful as a CLI tool -- a developer can sign and store feed messages from shell scripts, cron jobs, CI pipelines, or hooks without running the daemon process.

## CLI Interface

### Usage Examples

```bash
# Inline text (default content-type: insight)
egregore publish "Your AI agent discovered something interesting"

# With explicit content-type and topic
egregore publish --content-type insight --topic reasoning "Chain-of-thought result..."

# From file
egregore publish --file results.json --content-type annotation

# With threading (relates to another message)
egregore publish --relates abc123def456... "Follow-up observation"

# JSON mode (for scripting)
egregore --json publish "message text"

# Quiet mode (hash only on stdout, nothing else)
egregore --quiet publish "message text"
```

### Clap Struct

Add this variant to the existing `Command` enum in `src/main.rs`:

```rust
#[derive(clap::Subcommand, Debug)]
enum Command {
    // ... existing variants ...

    /// Publish a signed message to the local feed (no daemon required)
    Publish {
        /// Message content (text or JSON string).
        /// Mutually exclusive with --file.
        content: Option<String>,

        /// Read content from a file (JSON or plain text).
        /// Mutually exclusive with positional content.
        #[arg(long, conflicts_with = "content")]
        file: Option<PathBuf>,

        /// Content type for the message envelope.
        /// Determines the "type" field in the content JSON.
        /// [default: insight]
        #[arg(long, default_value = "insight")]
        content_type: String,

        /// Topic tag for the message.
        #[arg(long)]
        topic: Option<String>,

        /// Additional tags (comma-separated or repeated).
        #[arg(long)]
        tag: Vec<String>,

        /// Hash of a related message (for threading).
        #[arg(long)]
        relates: Option<String>,

        /// Schema identifier (e.g., "insight/v1").
        /// If omitted, inferred from content type.
        #[arg(long)]
        schema_id: Option<String>,
    },
}
```

Key decisions:

- `content` is `Option<String>` (not required) because `--file` can supply the content instead.
- `conflicts_with = "content"` on `--file` lets clap enforce mutual exclusivity.
- `--content-type` defaults to `"insight"` -- the most common type in the egregore ecosystem.
- `--topic` is a convenience for the most common single-tag pattern. `--tag` is for additional tags. Both contribute to the message's `tags` vec.

### Output Behavior

Controlled by existing global flags `--json` and `--quiet`:

| Mode | stdout |
|------|--------|
| Default (human) | `Published: <hash>` (single line) |
| `--json` | Full `Message` struct as JSON |
| `--quiet` | Nothing (but exit code 0 on success) |

The hash is always a 64-character hex SHA-256 string, so it is grep-able and pipe-safe. In default mode the output is a single line with no trailing whitespace, suitable for `$(egregore publish ...)` capture.

## Implementation Plan

### Step 1: Add the `Publish` variant to `Command` enum

**File:** `src/main.rs`

Add the struct shown above to the `Command` enum. No new dependencies required -- `PathBuf` is already imported.

### Step 2: Handle the Publish command in `cli_admin.rs`

**File:** `src/cli_admin.rs`

Add a new arm in `handle_command`:

```rust
Command::Publish {
    content,
    file,
    content_type,
    topic,
    tag,
    relates,
    schema_id,
} => {
    handle_publish(ctx, output, content, file, content_type, topic, tag, relates, schema_id)?;
    Ok(true)
}
```

Then implement `handle_publish`:

```rust
fn handle_publish(
    ctx: &CliContext,
    output: OutputMode,
    content_arg: &Option<String>,
    file: &Option<PathBuf>,
    content_type: &str,
    topic: &Option<String>,
    tags: &[String],
    relates: &Option<String>,
    schema_id: &Option<String>,
) -> anyhow::Result<()> {
    // 1. Resolve raw content string
    let raw = resolve_content(content_arg, file)?;

    // 2. Build content JSON value
    let content_value = build_content_json(&raw, content_type)?;

    // 3. Collect tags (topic + explicit tags)
    let mut all_tags: Vec<String> = tags.to_vec();
    if let Some(topic) = topic {
        if !all_tags.contains(topic) {
            all_tags.insert(0, topic.clone());
        }
    }

    // 4. Publish via FeedEngine
    let engine = FeedEngine::new(ctx.store.clone());
    let message = engine.publish_with_schema(
        &ctx.identity,
        content_value,
        schema_id.clone(),
        relates.clone(),
        all_tags,
    )?;

    // 5. Output
    output.emit(&message, || format!("Published: {}", message.hash))?;
    Ok(())
}
```

Two helper functions:

```rust
/// Resolve content from either positional arg or --file.
fn resolve_content(
    content_arg: &Option<String>,
    file: &Option<PathBuf>,
) -> anyhow::Result<String> {
    match (content_arg, file) {
        (Some(text), None) => Ok(text.clone()),
        (None, Some(path)) => {
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read content file: {}", path.display()))
        }
        (None, None) => bail!("provide content as argument or --file"),
        (Some(_), Some(_)) => unreachable!("clap enforces mutual exclusivity"),
    }
}

/// Build the content JSON value from raw text and content type.
///
/// If the raw string is valid JSON and already has a "type" field, use it as-is.
/// If the raw string is valid JSON without a "type" field, inject the content_type.
/// If the raw string is not valid JSON, wrap it in a typed envelope.
fn build_content_json(
    raw: &str,
    content_type: &str,
) -> anyhow::Result<serde_json::Value> {
    // Try parsing as JSON first
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(obj) = json.as_object_mut() {
            // Inject "type" if not already present
            obj.entry("type".to_string())
                .or_insert_with(|| serde_json::Value::String(content_type.to_string()));
        }
        return Ok(json);
    }

    // Plain text: wrap in a typed envelope
    Ok(build_text_envelope(raw, content_type))
}

/// Build a content envelope for plain text based on content type.
fn build_text_envelope(text: &str, content_type: &str) -> serde_json::Value {
    match content_type {
        "insight" => serde_json::json!({
            "type": "insight",
            "title": truncate_title(text, 80),
            "observation": text,
        }),
        "annotation" => serde_json::json!({
            "type": "annotation",
            "text": text,
        }),
        other => serde_json::json!({
            "type": other,
            "text": text,
        }),
    }
}

fn truncate_title(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        let truncated = &text[..text.floor_char_boundary(max_len.saturating_sub(3))];
        format!("{}...", truncated)
    }
}
```

### Step 3: Handle config validation for CLI-only commands

**File:** `src/main.rs`, function `build_config`

The current `build_config` calls `config.validate()` which enforces daemon constraints like `api_auth_token`. The `publish` subcommand does not need API, gossip, or network config. The existing admin commands (Follow, Identity, etc.) already pass through `build_config` without issue because they rely on the user having a config file.

The simplest fix: make `config.validate()` skip API/port validation when the config is used for a CLI-only operation. Two options:

**Option A (preferred): Skip validate for admin subcommands.**

The admin subcommand path already returns early before any daemon code runs. Move `config.validate()` out of `build_config` and into the daemon startup path:

```rust
// In build_config: remove the config.validate()? call

// In main(), after the admin subcommand early-return:
config.validate()?;
```

This is the right structural fix. Admin commands only need `data_dir` and `identity_dir` to be valid. They should not be gated by API auth token configuration.

**Option B (minimal): Add a `validate_for_admin()` method that only checks data_dir exists.**

Option A is preferred because it matches the existing flow -- admin commands already bail early before daemon startup, so moving validation to the daemon path is strictly correct.

## File Changes Summary

| File | Change |
|------|--------|
| `src/main.rs` | Add `Publish` variant to `Command` enum. Move `config.validate()` from `build_config` to daemon startup path. |
| `src/cli_admin.rs` | Add `Publish` arm to `handle_command`. Add `handle_publish`, `resolve_content`, `build_content_json`, `build_text_envelope`, `truncate_title` functions. Add `use egregore::feed::engine::FeedEngine;` import (already used in `handle_rotate_key`). |

No new files. No new dependencies.

## Data Flow

```
CLI args
  |
  v
clap parse -> Command::Publish { content, file, content_type, ... }
  |
  v
build_config() -> Config (data_dir, identity_dir, db_path)
  |
  v
Identity::load_or_generate(identity_dir)
  |
  v
FeedStore::open(db_path)          <-- SQLite WAL mode, 5s busy timeout
  |
  v
resolve_content(arg | file)       <-- raw string
  |
  v
build_content_json(raw, type)     <-- serde_json::Value with "type" field
  |
  v
FeedEngine::publish_with_schema() <-- handles:
  |                                    - sequence allocation (atomic)
  |                                    - previous hash chaining
  |                                    - SHA-256 content hash
  |                                    - Ed25519 signature
  |                                    - SQLite insert
  v
Message { hash, sequence, ... }
  |
  v
OutputMode::emit()                <-- hash to stdout
```

The `FeedEngine::publish_with_schema` method handles all the complex parts: atomic sequence allocation, previous hash lookup, unsigned message construction, SHA-256 hashing, Ed25519 signing, and SQLite insert. The CLI publish handler is thin glue between argument parsing and the engine.

## Edge Cases

### No identity exists yet

`Identity::load_or_generate` creates a new keypair on first use. This is the existing behavior for all subcommands. The publish command will generate identity on first run, same as `egregore identity` or `egregore follow`.

### Store does not exist yet

`FeedStore::open` creates the database and runs migrations if the file does not exist. First publish creates the store. This is existing behavior.

### Store is locked by running daemon

SQLite WAL mode allows concurrent access. `FeedStore::open` sets `busy_timeout(5s)`. If the daemon holds a write lock, the CLI will retry for up to 5 seconds. If the timeout expires, the user gets a clear SQLite "database is locked" error. This is an acceptable edge case -- the daemon's write transactions are short (single message insert).

### Neither content arg nor --file provided

`resolve_content` returns `bail!("provide content as argument or --file")`. Clear, actionable error.

### --file points to nonexistent file

`std::fs::read_to_string` returns an error with `with_context` formatting: `"failed to read content file: /path/to/file"`.

### Content exceeds 64 KB limit

`FeedEngine::publish_full` enforces `MAX_CONTENT_SIZE` (64 KB) and returns `EgreError::FeedIntegrity`. The error message includes the actual size and the limit.

### Invalid JSON in --file

If the file content is not valid JSON, `build_content_json` falls through to the text envelope path and wraps the raw text in a `{"type": "...", "text": "..."}` envelope. This is intentional -- the command should work with both JSON and plain text files.

### Content is valid JSON but not an object

If the user passes a JSON array or scalar, `build_content_json` returns it as-is (no "type" injection since there is no object to inject into). Schema validation in the engine will catch structurally invalid content if strict mode is enabled.

### Encrypted identity (--passphrase)

The existing `--passphrase` flag is handled in `main.rs` before dispatching to `cli_admin::handle_command`. The publish subcommand inherits this path automatically. If `--passphrase` is set, the user is prompted for their passphrase, the key is decrypted, and it is passed to `CliContext` like any other command.

### Key has been rotated

If the identity's signing key has been superseded by a `key_rotation` message, `FeedEngine::publish_full` does not check for this (rotation checks are on the ingest path only). Publishing with a rotated key is valid -- the message is locally signed and stored. Peers will reject it during replication if the rotation is effective, which is the correct behavior: the user needs to know their key was rotated.

This is actually a consideration for a future improvement (warn on publish with rotated key), but it is out of scope for this design.

## What NOT to Build

1. **No daemon communication.** The publish command writes directly to SQLite. It does not POST to the HTTP API, connect to gossip, or check if a daemon is running.

2. **No gossip trigger.** After publishing, the message sits in the local store. It propagates on the next gossip cycle when the daemon runs. Adding a "notify daemon" mechanism (e.g., Unix socket, inotify) is a separate feature.

3. **No stdin reading.** Reading from stdin (piped content) is a useful extension but adds complexity (detecting TTY vs pipe, buffering). The `--file /dev/stdin` workaround exists. Stdin support can be added later as `--file -` convention.

4. **No batch publish.** Publishing multiple messages from a JSONL file or directory is out of scope. Each invocation publishes one message.

5. **No --expires-at flag.** TTL/expiration is supported in the message model (`expires_at` field) but is not commonly used. Adding the flag is trivial later.

6. **No content type validation at the CLI level.** The CLI does not maintain a list of valid content types. Schema validation in the engine handles this (in strict mode). The CLI passes through whatever `--content-type` the user provides.

7. **No network key validation.** The publish command does not call `config.validate_security()`. Network key configuration is a daemon concern -- local publishing does not use the network key.

## Testing Strategy

### Unit Tests (in `cli_admin.rs`)

```rust
#[test]
fn resolve_content_from_arg() {
    let result = resolve_content(&Some("hello".to_string()), &None).unwrap();
    assert_eq!(result, "hello");
}

#[test]
fn resolve_content_neither_arg_nor_file() {
    let result = resolve_content(&None, &None);
    assert!(result.is_err());
}

#[test]
fn build_content_json_plain_text_insight() {
    let value = build_content_json("observed something", "insight").unwrap();
    assert_eq!(value["type"], "insight");
    assert_eq!(value["observation"], "observed something");
}

#[test]
fn build_content_json_existing_json_preserves_type() {
    let json = r#"{"type": "custom", "data": 42}"#;
    let value = build_content_json(json, "insight").unwrap();
    assert_eq!(value["type"], "custom"); // Does NOT override
}

#[test]
fn build_content_json_json_without_type_injects() {
    let json = r#"{"data": 42}"#;
    let value = build_content_json(json, "annotation").unwrap();
    assert_eq!(value["type"], "annotation"); // Injected
}

#[test]
fn truncate_title_short_text_unchanged() {
    assert_eq!(truncate_title("short", 80), "short");
}

#[test]
fn truncate_title_long_text_truncated() {
    let long = "a".repeat(100);
    let result = truncate_title(&long, 80);
    assert!(result.len() <= 80);
    assert!(result.ends_with("..."));
}
```

### Integration Test (in `src/main.rs` tests or integration test file)

```rust
#[test]
fn cli_parses_publish_with_all_flags() {
    let cli = Cli::try_parse_from([
        "egregore",
        "--json",
        "--data-dir", "/tmp/test",
        "publish",
        "--content-type", "annotation",
        "--topic", "reasoning",
        "--tag", "test",
        "--relates", "abc123",
        "Some content here",
    ]).unwrap();

    assert!(cli.json);
    match cli.command.unwrap() {
        Command::Publish {
            content,
            file,
            content_type,
            topic,
            tag,
            relates,
            schema_id,
        } => {
            assert_eq!(content.as_deref(), Some("Some content here"));
            assert!(file.is_none());
            assert_eq!(content_type, "annotation");
            assert_eq!(topic.as_deref(), Some("reasoning"));
            assert_eq!(tag, vec!["test".to_string()]);
            assert_eq!(relates.as_deref(), Some("abc123"));
            assert!(schema_id.is_none());
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn cli_parses_publish_with_file() {
    let cli = Cli::try_parse_from([
        "egregore",
        "publish",
        "--file", "results.json",
        "--content-type", "annotation",
    ]).unwrap();

    match cli.command.unwrap() {
        Command::Publish { content, file, .. } => {
            assert!(content.is_none());
            assert_eq!(file.unwrap().to_str().unwrap(), "results.json");
        }
        other => panic!("unexpected command: {other:?}"),
    }
}
```

### Smoke Test (end-to-end, pre-built binary)

```bash
#!/usr/bin/env bash
set -euo pipefail
TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

# Publish a message (generates identity + store on first run)
OUTPUT=$(egregore --data-dir "$TMPDIR" publish "test message" 2>/dev/null)
HASH=${OUTPUT#Published: }

# Verify hash is 64 hex chars
[[ ${#HASH} -eq 64 ]] || { echo "FAIL: hash length ${#HASH}"; exit 1; }
[[ "$HASH" =~ ^[0-9a-f]+$ ]] || { echo "FAIL: hash not hex"; exit 1; }

# Publish with JSON mode and verify output
JSON=$(egregore --json --data-dir "$TMPDIR" publish "second message")
SEQ=$(echo "$JSON" | jq -r '.sequence')
[[ "$SEQ" -eq 2 ]] || { echo "FAIL: expected sequence 2, got $SEQ"; exit 1; }

echo "PASS"
```

## Scope Estimate

- **Lines of code:** ~120 in `cli_admin.rs`, ~15 in `main.rs`
- **Risk:** Low. Uses existing `FeedEngine::publish_with_schema` which is well-tested (see engine.rs tests). The new code is thin argument parsing and content formatting.
- **Review focus:** The `build_content_json` function's JSON-vs-text detection logic. Verify that valid JSON with no "type" field gets the type injected correctly, and that non-JSON text is always wrapped.
