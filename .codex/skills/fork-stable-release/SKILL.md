---
name: fork-stable-release
description: Refresh this fork's stable branch from an upstream non-alpha rust-v release tag by branching from current stable, merging the upstream tag into that branch, and resolving conflicts while preserving fork-only features.
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

4. Create the refresh branch from current fork `stable`.

Always branch from the branch you plan to merge into:

```bash
git checkout stable
git pull --ff-only
git checkout -b stable-refresh/<tag>-from-stable
```

5. Merge the upstream release tag into the stable-based branch.

```bash
git merge <tag>^{}
```

Do not merge fork `main` into `stable`. Do not start from the upstream tag and
then replay fork commits unless the user explicitly asks for that recovery
strategy. The normal path is a stable-based merge so the current released fork
state remains the compatibility contract.

6. Resolve conflicts by preserving the maintained fork contract.

Do not assume the correct result is "whatever upstream picked". Treat the
previous `stable` branch as the compatibility contract and inventory the
fork-only surfaces that must survive the refresh.

At minimum:

- read `docs/fork-differences.md`
- compare old `stable` against the incoming upstream tag where conflicts touch fork-owned code
- enumerate fork-owned runtime, packaging, config, and UX surfaces
- classify each surface as `preserved`, `adapted`, or `intentionally dropped`
- stop if any surface is still unclassified

Keep this inventory in the PR body or working notes so the merge decision is reviewable.

## Maintained Fork Layer

The stable branch should stay intentionally small, but it is still a product surface with compatibility expectations. Do not guess what belongs in the fork layer from file paths alone.

The maintained fork layer must be kept current in this skill. If the fork picks up or drops stable-only behavior, update this section in the same branch.

As of the current branch, the fork contract includes at least:

- fork packaging/versioning surfaces for the `codex-rick` line
- account alias switching and keychain/file auth compatibility
- Orchestrator and Continuous mode behavior, including CLI startup mode selection
- Orchestrator model defaults/fallbacks, spawn safety, primary contact polling, session overwatch, and child completion hooks
- orchestrator memory, memory helper naming, cleanup/consolidation, and slash commands
- built-in scratchpad, built-in schedule, compaction recovery, and resume/fast-resume integration
- MCP behavior with mode enablement filters, startup cancellation retry, missing-tool recovery, and inline Orchestrator communication/state MCP use
- fork docs and skill docs that teach future agents how to preserve the fork

When refreshing `stable`, prefer carrying forward commits that stay inside those surfaces or are clearly required to preserve them.

If a candidate commit touches broader runtime areas, review it explicitly and map it back to one of the fork-contract surfaces before carrying it forward.

## State Migration Policy

State migrations are versioned by the numeric filename prefix and applied
migrations validate checksums on startup. Once a migration ships on `stable`, do
not rename it, renumber it, or change its SQL.

Fork-only migrations should use the next unused version and include `rick` in
the filename so future upstream refreshes can identify the source quickly:

```text
0031_rick_short_feature_name.sql
```

If an upstream release adds a migration number that collides with an
already-shipped fork migration, keep the shipped fork migration exactly as-is and
move the upstream migration to the next unused version:

```text
0028_rick_existing_fork_feature.sql   # already shipped, do not change
0029_upstream_original_feature.sql    # upstream migration moved forward
```

The SQL objects may not conflict even when filenames do. Check both separately:

- migration-number conflict: preserve stable's shipped filename/checksum
- SQL object conflict: resolve with normal schema review

For this repo, also keep `codex-rs/state/migrations/README.md` and
`docs/fork-differences.md` aligned with the current migration policy.

## Conflict Policy

Resolve conflicts locally when they are narrow and clearly mechanical, especially:

- workspace dependency wiring
- crate manifest updates that only need to keep both upstream and fork dependencies
- generated schema files after resolving source changes
- app-server/core protocol drift where both upstream API additions and fork behavior can coexist
- state migration number collisions that can be resolved by preserving shipped fork versions and moving unshipped upstream migrations forward

Stop and ask the user before proceeding when any of the following happens:

- conflicts spread outside the maintained fork layer or the fork contract is no longer clear
- more than a few files conflict across unrelated crates
- upstream changed versioning or release metadata semantics enough that the fork patch is no longer obviously correct
- replaying the fork commits would require new product decisions instead of straightforward adaptation
- the audit shows a previously maintained stable surface that now needs to be intentionally dropped or redesigned

## Verification

After resolving the upstream merge:

1. Run `just fmt` in `codex-rs` if Rust files changed.
2. Run the focused tests for the changed area.
3. Run proof-oriented checks for the fork surfaces that changed.

At minimum, validation should include the relevant subset of:

```bash
cd codex-rs
cargo check -p codex-core
cargo check -p codex-app-server
cargo check -p codex-tui
cargo test -p codex-app-server-protocol
```

Examples of required proofs when those surfaces are in scope:

- release/CLI compile path for packaging changes
- MCP startup cancel/retry regression proof
- lazy MCP visibility/loading checks
- Orchestrator mode entry/default behavior
- Continuous mode interrupt behavior
- built-in scratchpad and compaction recovery behavior
- state migration compatibility, especially that already-shipped stable migration checksums did not change

4. Before merging into `stable`, confirm all of the following:

- the fork-contract inventory is complete
- each maintained surface is marked `preserved`, `adapted`, or `intentionally dropped`
- any remaining red CI is explicitly classified as code-related or infra-only
- no known fork surface is missing from the refreshed branch

5. If the branch is meant to replace `stable`, push it to the fork and report:

- the upstream tag used
- the merge branch and merge strategy used
- the fork-contract surfaces reviewed
- any conflicts that were resolved
- any upstream release behavior intentionally skipped in the fork, such as private-infra workflows
- any remaining infra-only CI failures that did not block the merge

## Notes

- Treat `stable` as the install base for local builds that should track upstream releases conservatively.
- Treat fork `main` as experimental unless the user explicitly wants to work from upstream development head.
- When the maintained fork layer changes, update this skill in the same branch so future refreshes know what to preserve.
- If this skill is out of date, stop and fix the skill before trusting it for the next release refresh.
