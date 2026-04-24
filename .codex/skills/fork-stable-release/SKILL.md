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

6. Audit the maintained fork contract before merging.

Do not assume the fork layer is only "whatever cherry-picks cleanly". Treat the previous `stable` branch as the compatibility contract and inventory the fork-only surfaces that must survive the refresh.

At minimum:

- diff the old `stable` branch against the old upstream tag
- enumerate the fork-owned runtime, packaging, config, and UX surfaces
- classify each surface as `preserved`, `adapted`, or `intentionally dropped`
- stop if any surface is still unclassified

Keep this inventory in the PR body or working notes so the merge decision is reviewable.

## Maintained Fork Layer

The stable branch should stay intentionally small, but it is still a product surface with compatibility expectations. Do not guess what belongs in the fork layer from file paths alone.

The maintained fork layer must be kept current in this skill. If the fork picks up or drops stable-only behavior, update this section in the same branch.

As of the current branch, the fork contract includes at least:

- `codex-rs/build-info/**` for Rick-specific source-build version stamping
- `codex-rs/features/**` and downstream consumers for fork-only feature metadata such as `FeatureOwner` and related user-facing experimental help
- stable-only MCP behavior that has explicit user-facing expectations, including lazy loading, startup cancellation handling, and related regression proofs
- stable-only thread-control behavior, including Orchestrator mode, Continuous mode, and `Esc` interrupt semantics
- fork packaging/versioning surfaces for the `codex-rick` line
- `.codex/skills/fork-stable-release/**` for this maintenance workflow

When refreshing `stable`, prefer carrying forward commits that stay inside those surfaces or are clearly required to preserve them.

If a candidate commit touches broader runtime areas, review it explicitly and map it back to one of the fork-contract surfaces before carrying it forward.

## Conflict Policy

Resolve conflicts locally when they are narrow and clearly mechanical, especially:

- workspace dependency wiring
- crate manifest updates that only need to keep both upstream and fork dependencies
- build-info implementation drift inside `codex-rs/build-info`

Stop and ask the user before proceeding when any of the following happens:

- conflicts spread outside the maintained fork layer or the fork contract is no longer clear
- more than a few files conflict across unrelated crates
- upstream changed versioning or release metadata semantics enough that the fork patch is no longer obviously correct
- replaying the fork commits would require new product decisions instead of straightforward adaptation
- the audit shows a previously maintained stable surface that now needs to be intentionally dropped or redesigned

## Verification

After replaying the fork layer:

1. Run `just fmt` in `codex-rs` if Rust files changed.
2. Run the focused tests for the changed area.
3. Run proof-oriented checks for the fork surfaces that changed.

At minimum, validation should include the relevant subset of:

```bash
cd codex-rs
cargo test -p codex-build-info
```

Examples of required proofs when those surfaces are in scope:

- release/CLI compile path for feature metadata and packaging changes
- MCP startup cancel/retry regression proof
- lazy MCP visibility/loading checks
- Orchestrator mode entry/default behavior
- Continuous mode interrupt behavior

4. Before merging into `stable`, confirm all of the following:

- the fork-contract inventory is complete
- each maintained surface is marked `preserved`, `adapted`, or `intentionally dropped`
- any remaining red CI is explicitly classified as code-related or infra-only
- no known fork surface is missing from the refreshed branch

5. If the branch is meant to replace `stable`, push it to the fork and report:

- the upstream tag used
- the commits replayed
- the fork-contract surfaces reviewed
- any conflicts that were resolved
- any commits intentionally left behind
- any remaining infra-only CI failures that did not block the merge

## Notes

- Treat `stable` as the install base for local builds that should track upstream releases conservatively.
- Treat fork `main` as experimental unless the user explicitly wants to work from upstream development head.
- When the maintained fork layer changes, update this skill in the same branch so future refreshes know what to preserve.
- If this skill is out of date, stop and fix the skill before trusting it for the next release refresh.
