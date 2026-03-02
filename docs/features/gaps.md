# Documentation Gap Tracker

This file tracks cross-feature gaps discovered while writing feature-isolated docs.

## Open Gaps

1. HTTP auth and TLS are edge-only today.
   - In-process auth/TLS options are not available.
   - If direct network exposure is needed, this likely needs product/design work.

2. Follows/topics are API-managed only.
   - No CLI flags or `config.yaml` bootstrap entries for follows/topics.

3. Consumer groups are API-managed only.
   - No CLI workflow and no MCP tools for group operations.

4. Schema registry operations are API-managed only.
   - No CLI workflow and no MCP tools for schema operations.

5. Retention policy management is API-managed only.
   - No CLI workflow and no MCP tools for retention policy CRUD.

6. Topic controls are not exposed in MCP.
   - MCP has follow/unfollow tools but no topic subscribe/unsubscribe/list tools.

7. Private Box is library-only.
   - No HTTP/MCP route currently exposes this capability.

8. Feature docs currently include usage patterns and examples but not troubleshooting runbooks per feature.
   - Troubleshooting remains centralized in `docs/operations.md`.

## Suggested Review Order

1. Security boundary decisions (auth/TLS, exposure model)
2. CLI and MCP parity for policy/governance features
3. Private Box productization path
4. Feature-specific troubleshooting additions
