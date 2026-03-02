# Architecture Slice: Consumer Groups

## Scope

Coordinates multiple local workers for feed-processing ownership and progress tracking.

## Primary Modules

- `src/feed/store/groups.rs`
- `src/api/routes_groups.rs`

## Core Design

- Group members join/leave locally on one node.
- Assignment partitions author feeds across members (round-robin).
- Offsets are committed per `(group, author)`.
- Generation increments detect stale membership/assignment views.

## Invariants

- Groups are local state, not cross-node consensus.
- Assignment ownership is enforced on commit.
- Membership changes trigger rebalance.

## Storage Ownership

- `consumer_groups`, `group_members`, `group_assignments`, `group_offsets`

## Related Usage Doc

- `docs/features/consumer-groups.md`
