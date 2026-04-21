# Fork differences

This fork tracks [`openai/codex`](https://github.com/openai/codex) and keeps a
small set of intentional differences on top.

Use this page as the index for anything that exists only in `@rickgetz/codex`
or behaves differently from upstream.

## Current differences

### Distribution

- npm package: `@rickgetz/codex`
- Primary install command: `npm install -g @rickgetz/codex`
- Installed executable: `codex-rick`
- The upstream `@openai/codex` package can remain installed as `codex` for
  fallback use.
- GitHub releases come from this fork, not the upstream OpenAI repository.

### Release lane

- Merges or pushes to `stable` automatically produce fork releases.
- Fork versions use the format `<upstream version>-rick.<counter>`.
- Git tags use the format `rick-v<upstream version>-rick.<counter>`.
- The automated release lane currently publishes Apple Silicon macOS binaries only.

See [Fork npm releases](./fork-release.md) for the release workflow details.

### Feature toggles

- This fork carries `enable_mcp_approvals` as a Rick-owned feature.
- Persist it in config with:
  - `codex features enable enable_mcp_approvals`
  - `codex features disable enable_mcp_approvals`
- Override it for one run with:
  - `codex --enable enable_mcp_approvals`
  - `codex --disable enable_mcp_approvals`
- `codex features list` marks Rick-owned features with `(rick)`.

## Fork-only feature labeling

If this fork adds an experimental feature that surfaces its own help text in the
UI or app-server metadata, that help text must be labeled with a `(rick)` prefix.

The enforcement point for that lives in
`codex-rs/features/src/lib.rs`:

- experimental features declare an explicit `owner`
- `FeatureOwner::Rick` automatically prefixes user-facing descriptions and announcements with `(rick)`

That means new fork-only experimental features should:

1. set `owner: FeatureOwner::Rick`
2. add or update an entry on this page if the feature changes fork behavior

Do not add entries here for intended differences that are not actually active in
this fork yet.
