---
version: "1.4"
lastUpdated: "2026-02-28 UTC"
lifecycle: "active"
stakeholder: "technical"
changeTrigger: "Prebuilt binary deployment, source-free node operation"
validatedBy: "user"
dependencies: ["activeContext.md", "techEnvironment.md"]
---

# workflowProtocols

## Memory Location and Tool Scope

- Memory path: `Memory/` (relative), `/home/pknull/Code/egregore/Memory/` (absolute)
- Access rule: Read/Write tools for Memory files, Bash for git operations

## Technical Verification

- **Build**: `cargo build --release`
- **Test**: `cargo test`
- **Lint**: `cargo clippy`

## Infrastructure Validation Protocol

**BEFORE recommending new capabilities, commands, or infrastructure**:

1. **Check existing infrastructure** against proposed enhancement
2. **Compare proposed vs existing**: What's genuinely new?
3. **Validate transferability**: Does this pattern work in our domain?

**Pitfall**: Recommending duplicative infrastructure without checking existing capabilities.

**Prevention**: Always ask "How does this compare to what we already have?"

## Node Deployment Protocol

**Binary update** (prebuilt):
1. `egregore update` (self-update from GitHub releases)
2. Restart service: `systemctl --user restart egregore`
3. Verify: `systemctl --user status egregore` then `egregore_status` via MCP

**From source** (development only):
1. `cargo build --release`
2. `cp target/release/egregore ~/.local/bin/egregore`
3. Restart + verify as above

**Systemd service**: `~/.config/systemd/user/egregore.service`
- `ExecStart=%h/.local/bin/egregore --data-dir %h/egregore-data`
- `UnsetEnvironment=CLAUDECODE CLAUDE_CODE_ENTRYPOINT` prevents nested Claude session errors
- Enabled at login: `systemctl --user enable egregore`
- Logs: `journalctl --user -u egregore -f`

**Runtime layout** (independent of source checkout):
- Binary: `~/.local/bin/egregore`
- Hooks: `~/.egregore/hooks/`
- Python venv: `~/.egregore/venv/`
- Docker: `~/.egregore/docker/`
- Data: `/home/pknull/egregore-data/`

## Project-Specific Protocols

- **Mesh operations**: Check feed via `egregore_query` before acting. Confirm destructive ops with operator.
- **Firewall/UDM**: Use `~/Containers/unifi/udm-api.sh`, NOT local iptables/ufw.
- **Webhook integrations**: Keep native `--hook-webhook-url` generic. Use `--hook-on-message` with formatting scripts for service-specific APIs.
- **Multi-hook support**: Daemon config file supports multiple hook entries (no longer limited to one `--hook-on-message`).
- **Infrastructure actions**: Watcher runs Claude from Containers project directory to inherit docker/firewall permissions. No custom MCP server needed.

## Validated Patterns

### Hook Dispatcher for Multiple Handlers

**When to Use**: Need multiple actions on message arrival (Discord, Claude watcher, logging, etc.)
**Process**: Single `on-message.sh` reads stdin, pipes to handlers in parallel via `&`, then `wait`
**Why This Works**: Hook executor only supports one script path. Dispatcher provides fan-out without daemon changes.
**Anti-Pattern**: Running multiple daemons or using cron alongside hooks.

### Clean Env for Daemon Startup

**When to Use**: Starting egregore from any Claude session (interactive or CI)
**Process**: `env -u CLAUDECODE -u CLAUDE_CODE_ENTRYPOINT nohup egregore ...`
**Why This Works**: Claude sets `CLAUDECODE=1` in its process tree. Hook scripts that invoke Claude CLI detect this and refuse to run.
**Anti-Pattern**: Starting daemon without stripping env, then debugging "nested session" errors in hook scripts.

### Check the Mesh Before Acting

**When to Use**: User references something happening "elsewhere" or mentions the other node
**Process**: `egregore_query` with recent limit to see latest feed activity
**Why This Works**: Peer nodes post status updates, feature proposals, and requests to the feed
**Anti-Pattern**: Assuming full context without checking the mesh.

### Hook Script PATH Requirements

**When to Use**: Any hook script that calls tools not in /usr/bin
**Process**: Export full PATH at top of script, including `~/.local/bin` and common tool paths
**Why This Works**: Systemd service doesn't inherit user's interactive shell PATH
**Anti-Pattern**: Assuming PATH is available, then getting exit 127 from missing `claude`, `jq`, etc.

### Broadcast Channel Receiver Pattern

**When to Use**: Any `tokio::sync::broadcast::Receiver` in a loop
**Process**: Match on `Lagged` (log + continue) and `Closed` (break). Never use `while let Ok(msg)`.
**Why This Works**: `Lagged` is recoverable — just means some messages were dropped. `Closed` means shutdown.
**Anti-Pattern**: `while let Ok(msg) = rx.recv().await` — silently kills the receiver on buffer overflow.

### Reuse Existing Project Permissions

**When to Use**: Need Claude to perform privileged operations (docker, firewall, etc.)
**Process**: Run Claude from a project directory that already has `settings.local.json` with the needed Bash permissions.
**Why This Works**: Claude Code reads project-level settings including allowedTools. No need to build custom MCP servers.
**Anti-Pattern**: Building new MCP servers or tool wrappers when permissions already exist in another project.
