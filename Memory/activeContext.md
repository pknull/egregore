---
version: "2.0"
lastUpdated: "2026-02-28 UTC"
lifecycle: "active"
stakeholder: "all"
changeTrigger: "Session save: prebuilt binary migration, source-free deployment"
validatedBy: "user"
dependencies: ["communicationStyle.md"]
---

# activeContext

## Current Project Status

**Primary Focus**: Egregore — decentralized knowledge sharing network for LLMs with gossip replication

**Active Work**:
- v1.1.0 running from `~/.local/bin/egregore` (prebuilt binary, self-update capable)
- Claude Agent SDK hook (`~/.egregore/hooks/claude-disciplined-hook.py`) for mesh auto-response
- Systemd-managed daemon with clean environment
- Multi-node mesh operations (4 peers, 9 feeds, 1002 messages)
- Source checkout no longer required to run the node

**Recent Activities** (last 7 days):
- **2026-02-28 (session 11)**: Migrated from source build to prebuilt binary. Installed v1.1.0 to `~/.local/bin/egregore`. Copied hooks to `~/.egregore/hooks/` with dedicated venv at `~/.egregore/venv/`. Updated systemd service and config.yaml to use new paths. Rewrote Dockerfile to download prebuilt binary from GitHub releases (PR #59, merged). Docker now independent of source checkout.
- **2026-02-27 (session 10)**: Upgraded to v0.4.0 (push-based replication, relates/tags). Switched from bash watcher to `claude-disciplined-hook.py` using Claude Agent SDK. Installed Python 3.13 via asdf for SDK dependency. Removed hardcoded paths from all 5 example hook scripts (PR #34). Added `.claude/` to `.gitignore`. Broadcast `Lagged` fix merged (PR #28). End-to-end hook test confirmed working.
- **2026-02-22 (session 9)**: Merged Docker example PR #16. Upgraded to v0.2.0. Systemd service. Author allowlist. Infra actions pipeline. Broadcast `Lagged` bug fix.

## Key Learnings

- **CLAUDECODE env var**: Propagates to child processes, prevents nested Claude. Fix: systemd `UnsetEnvironment` directive.
- **Systemd user service**: Proper daemon management with `UnsetEnvironment=CLAUDECODE CLAUDE_CODE_ENTRYPOINT`. Replaces `nohup` approach.
- **Broadcast channel `Lagged` bug**: `while let Ok(msg)` on `tokio::sync::broadcast::Receiver` exits the loop on `Lagged` errors, permanently killing hook processing. Must handle `Lagged` vs `Closed` explicitly.
- **Claude Agent SDK hooks**: Python-based hooks using `claude-agent-sdk` package. Needs Python 3.10+ (installed 3.13 via asdf). Uses Claude Code credentials — no API key needed for Pro/Max. Execution discipline prompt distinguishes informational vs action queries.
- **Portable shebangs**: Always `#!/usr/bin/env python3`. Never hardcode venv or system paths in committed scripts.
- **Successful hook runs invisible at INFO**: Hook executor logs success at `debug` level. State file mtime is the best indicator of watcher activity.
- **Firewall on UDM**: Use `~/Containers/unifi/udm-api.sh`, not local iptables/ufw.

## Critical Reference Information

- **Node binary**: `~/.local/bin/egregore` (prebuilt, self-update via `egregore update`)
- **Node data**: `/home/pknull/egregore-data`
- **Node identity**: `@+K6IB8+Zrk0/3tOxFyQDkrBbqDHeY20ARX4B9cJyxw0=.ed25519`
- **Systemd service**: `~/.config/systemd/user/egregore.service` (enabled, auto-start)
- **Hook scripts**: `~/.egregore/hooks/discord-notify.sh`, `~/.egregore/hooks/claude-disciplined-hook.py`
- **Python venv**: `~/.egregore/venv/` (Python 3.13 via asdf, claude-agent-sdk + httpx)
- **Docker files**: `~/.egregore/docker/` (Dockerfile, docker-compose.yml, config.yaml)
- **Trusted authors**: lintop (`@7JIN8`), noctis (`@4epw+`), homebox2 (`@+K6IB`), docker relay (`@cL51n`)
- **Discord webhook URL**: `~/.egregore/discord-webhook.url`
- **UDM API**: `~/Containers/unifi/udm-api.sh`
- **GitHub**: https://github.com/pknull/egregore (private)
- **Daemon ports**: HTTP 7654, Gossip 7655

## Next Steps

**Immediate**:
- [ ] Run `egregore update` to upgrade from v1.1.0 → v1.1.2
- [ ] Test claude-disciplined-hook with a real query from another node
- [ ] Consider adding `RUST_LOG=egregore::hooks=debug` to see successful hook completions

**Deferred**:
- Infra actions via hook (container lifecycle, firewall — requires expanding hook capabilities)
- Firewall management tools (phase 3 — UDM port forwarding via mesh requests)
- Rate limiting, feed pruning, Web UI
