---
name: fork-stable-release
description: Maintain this fork's stable branch by rebasing it onto the latest upstream non-alpha rust-v release tag, then replaying only the intentional fork-only commits instead of merging fork main.
---

## When To Use

Use this skill in the `codex` fork when the user wants to:

- create or refresh the fork-local `stable` branch
- move `stable` to the latest upstream release tag
- carry forward Rick-specific fork changes without merging fork `main`
- resolve conflicts during a stable-branch upgrade

Do not use this skill for normal feature work or for syncing fork `main` with upstream `main`.

## Workflow

1. Verify a clean working tree with `git status --porcelain`.
2. Determine the latest stable upstream tag by looking for the newest non-alpha `rust-vX.Y.Z` tag. Do not use `main` as the base.
3. Fetch the tag from upstream.

If an `upstream` remote exists, prefer:

```bash
git fetch upstream --tags --prune
```

If the repo cannot persist an `upstream` remote, fetch directly from the URL:

```bash
git fetch https://github.com/openai/codex.git refs/tags/<tag>:refs/tags/<tag>
```

4. Create or refresh a branch from the tag instead of merging fork `main`.

For a new stable branch:

```bash
git checkout -b stable <tag>
```

For an update branch:

```bash
git checkout -b stable-refresh/<tag> <tag>
```

5. Replay only the intentional fork-only commits.

Never merge fork `main` into `stable`. Use cherry-picks so upstream development-only changes do not leak into the stable line.

List the fork-only delta relative to the current stable base:

```bash
git log --oneline --reverse <tag>..stable
```

Then pick only the commits that are part of the maintained fork layer.

## Maintained Fork Layer

The stable branch should stay intentionally small. The current fork-only surface is:

- `codex-rs/build-info/**` for Rick-specific source-build version stamping
- `.codex/skills/fork-stable-release/**` for this maintenance workflow

When refreshing stable, prefer carrying forward commits that stay inside that surface.

If a candidate commit touches broader runtime areas, review it explicitly before carrying it forward.

## Conflict Policy

Resolve conflicts locally when they are narrow and clearly mechanical, especially:

- workspace dependency wiring
- crate manifest updates that only need to keep both upstream and fork dependencies
- build-info implementation drift inside `codex-rs/build-info`

Stop and ask the user before proceeding when any of the following happens:

- conflicts spread outside the maintained fork layer
- more than a few files conflict across unrelated crates
- upstream changed versioning or release metadata semantics enough that the fork patch is no longer obviously correct
- replaying the fork commits would require new product decisions instead of straightforward adaptation

## Verification

After replaying the fork layer:

1. Run `just fmt` in `codex-rs` if Rust files changed.
2. Run the focused tests for the changed area.

For the current fork layer:

```bash
cd codex-rs
cargo test -p codex-build-info
```

3. If the branch is meant to replace `stable`, push it to the fork and report:

- the upstream tag used
- the commits replayed
- any conflicts that were resolved
- any commits intentionally left behind

## Notes

- Treat `stable` as the install base for local builds that should track upstream releases conservatively.
- Treat fork `main` as experimental unless the user explicitly wants to work from upstream development head.
- When the maintained fork layer changes, update this skill in the same branch so future refreshes know what to preserve.
