# Decisions

- The local Egregore node is the sole network signing principal: one
  Ed25519 keypair per node; clients publish through it and hold no network
  keypair of their own.
- main is protected: pull request plus green CI (Format, Clippy, Test,
  Build) to merge. No direct pushes.
- Subprocess hooks are deprecated and default-off behind
  allow_subprocess_hooks; webhooks are compatibility-only until they meet
  RFC 0003 §6.
- Consumer groups stay deleted; reintroduction requires a new RFC.
- Fail-closed allowlists: empty means none (trusted peers, guild allowlist,
  hooks).
- Distribution is GitHub release assets; deployment hosts never build
  Egregore locally.
- This repo carries its own Memory v2 pair; cross-component decisions live
  in the Thallus umbrella. Machine-local state stays under ignored Work/.
