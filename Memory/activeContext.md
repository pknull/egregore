# Objective

Egregore is the pipe of Thallus: a node daemon holding signed append-only
feeds with gossip replication, the sole network signing principal. Familiar,
Servitor and Scry publish and observe through their local node; it moves
messages and owns identity, never reasoning or execution.

# State

Verified 2026-09-15. Version 2.0.1; main at 6a2922f (merge of PR #141,
ci-flake-b8: mock transports wait for subscription registration); CI green
on main (Format, Clippy, Test, Build), which branch protection requires
before merge. RFC 0001 (transport abstraction) and 0002 are Implemented;
the bus/bridge/relay transport they describe is enabled by no configuration
and the umbrella retired its deployment artifacts on 2026-09-03, so RFC
requirements RL8 and RL11 stand unsatisfied pending the adoption review.
Subprocess hooks are deprecated and default-off behind
allow_subprocess_hooks; consumer groups are deleted. Deployments consume
release assets from GitHub, never local builds. This repo now carries its own
Memory v2 pair; cross-component decisions stay in the Thallus umbrella
(private repo pknull/Thallus).

# Next

- Bus/bridge adoption review ~2026-10-24 (RFC 0001 §10.2): retain or
  deprecate from measured evidence; amend RFC 0001/0002 accordingly.
- Any change lands through a PR with green CI; main is protected.

# Blockers

- None.
