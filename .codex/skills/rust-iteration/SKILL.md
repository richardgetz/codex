---
name: rust-iteration
description: Choose and run a fast, evidence-matched Rust build or test loop for changes in this Codex repository. Use when validating codex-rs edits or diagnosing a local Rust/CI failure; follow AGENTS.md for mandatory commands and coordination.
---

# Rust Iteration

Use this skill to decide which single validation run answers the current question and what evidence justifies another run. The repository's `AGENTS.md` owns required command, build-owner, formatting, and test-scope rules; follow it when it is more specific.

## Choose the proof before the command

Write down the claim the run must establish. A successful result proves only its actual scope. For diagnosis, name the competing hypotheses and choose one run whose output distinguishes them.

| Question | Smallest useful evidence |
| --- | --- |
| “Can I run a local Codex build?” | `just codex` builds the CLI and code-mode host, then launches Codex. Treat this as a usable-build check, not behavior coverage. |
| “Does this Rust production target compile?” | Use `cargo check -p <crate>` for the narrowest affected consumer; use `just codex` when the CLI and code-mode host are the intended consumers. Include downstream consumers only when the change reaches them. |
| “Does this behavior work?” | `just test -p <crate> <test-filter>` for a focused regression test. The package/target controls what compiles; the filter only narrows which tests run. |
| “Did an API/schema change regenerate correctly?” | Run the relevant generator once after the source shape is settled, inspect the generated diff, then validate its narrow consumer. |
| “Is the final patch formatted/lint-clean?” | Always run required `just fmt` after code edits. For a large Rust change, run scoped `just fix -p <project>` before the final `just fmt`, as required by `AGENTS.md`. |

Do not substitute one kind of proof for another. A successful app build does not establish test-only behavior, and a passing focused test does not prove a separate binary consumer compiles when it is outside that test target.

## One focused iteration

1. **Reuse evidence first.** Check whether this exact source/config revision already passed the same target and scope. Reuse that result; do not rerun it to get a fresher timestamp.
2. **Prove the oracle, fixture, and observer.** Trace state predicates to the source that produces them. When substituting a store, transport, mock, or scheduler, verify it preserves prerequisite operations and event order; otherwise the test may fail before reaching the target path. Check where matchers and filters sit relative to recorders: a filtered recorder cannot prove that no request occurred. With equal-length waits, identify which clock/event the test advances and which timeout it observes (for example, tool delay versus request-wait timeout). Do not compile a speculative diagnostic test to discover a guessed predicate.
3. **Batch edits before compiling.** Finish related implementation, test, fixture, and call-site edits; inspect the complete diff and relevant compiler-sensitive call sites once before starting Rust. Do not use repeated builds as a substitute for reading diagnostics or reviewing the changed code.
4. **Assign one build owner.** Integrate intended edits before building, use the repository's shared Cargo target/cache, serialize Rust commands, and freeze the source while validation is running. Do not launch a duplicate command because output is quiet or a lock is taking time.
5. **Keep a run record and report terminal events immediately.** Before launch, capture the integrated SHA, exact command and package/target/filter, build owner, target/cache location, and session or log handle. Retain and wait on that same command session. As soon as it terminates, report the SHA, command, session/log, exit status, first meaningful failure (or pass scope), interpretation, and next action to the owner/Lead. Do not leave a terminal result unreported while continuing other work.
6. **Triage before editing.** Classify the result as production compilation, `cfg(test)` compilation, a test assertion/runtime failure, generated-fixture drift, or infrastructure. Start from the earliest actionable diagnostic rather than cascaded errors, and verify which code path the selected target compiled.
7. **Repair once, then rerun once for the new revision.** Group all known related compiler/test fixes into one patch, review it, record the new SHA, and repeat the narrow proof. Retry the same command only when logs support an infrastructure/flaky diagnosis. Compare with a baseline only when evidence points to a regression and the comparison can resolve a specific hypothesis; then report the result rather than changing the PR to chase unrelated failures.
8. **Stop when the proof is complete.** Do not broaden to workspace-wide tests without explicit user approval. Add a narrower consumer check only for a concrete coverage gap, dependency, or diagnosed failure. Record any remaining unverified surface explicitly.

## After semantic validation

Run `just fix -p <project>` only when the change is large enough to require it under `AGENTS.md`; always run `just fmt` after code edits. Run each required finalization step once, in the prescribed order. Mechanical changes do not justify repeating a completed semantic test loop. If finalization exposes or introduces a substantive code change, report that the final source revision is not covered by the earlier result and follow the repository's validation rules.

Keep PR CI evidence tied to its head SHA. A green run on an older commit does not validate a later push; a new commit should trigger CI for the new revision.
