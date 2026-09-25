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
| “Is the final patch formatted/lint-clean?” | For a large Rust change, run `just fix -p <project>` when no shared crate changed; if a shared crate changed, use unscoped `just fix` as required by `AGENTS.md`. Then run required `just fmt` after code edits. |

Do not substitute one kind of proof for another. A successful app build does not establish test-only behavior, and a passing focused test does not prove a separate binary consumer compiles when it is outside that test target.

## One focused iteration

1. **Reuse evidence first.** Check whether this exact source/config revision already passed the same target and scope. Reuse that result; do not rerun it to get a fresher timestamp.
2. **Audit async integration tests before the first Rust run.** Trace the full timeline from submitted operation through request capture, scripted response, event delivery, Worker state transition, and expected Lead/root wake. Inspect the involved helpers to learn what state or event each observes, whether receiver reads consume events, and how it times out. Inventory every event wait, fixed sleep/delay, timeout, consuming receiver/drain, and terminal-state assertion; record its trigger, expected terminal state, and bound. For each asserted release or wake path, map the production transition to the scripted response/event order and confirm the fixture can reach it. Review all waits and fixture gates together, batch corrections, then run the narrowest useful target. Do not respond to a timeout by merely lengthening a sleep; first identify the missing transition and verify the fixture can produce it.
3. **Batch edits before compiling.** Finish related implementation, test, fixture, and call-site edits; inspect the complete diff and relevant compiler-sensitive call sites once before starting Rust. Do not use repeated builds as a substitute for reading diagnostics or reviewing the changed code.
4. **Assign one build owner.** Integrate intended edits before building, use the repository's shared Cargo target/cache, serialize Rust commands, and freeze the source while validation is running. Do not launch a duplicate command because output is quiet or a lock is taking time.
5. **Keep a run record and report terminal events immediately.** Before launch, capture the integrated SHA, exact command and package/target/filter, build owner, target/cache location, and session or log handle. Retain and wait on that same command session. As soon as it terminates, report the SHA, command, session/log, exit status, first meaningful failure (or pass scope), interpretation, and next action to the owner/Lead. Do not leave a terminal result unreported while continuing other work.
6. **Triage before editing.** Classify the result as production compilation, `cfg(test)` compilation, a test assertion/runtime failure, generated-fixture drift, or infrastructure. Start from the earliest actionable diagnostic rather than cascaded errors, and verify which code path the selected target compiled.
7. **Repair once, then rerun once for the new revision.** Group all known related compiler/test fixes into one patch, review it, record the new SHA, and repeat the narrow proof. Retry the same command only when logs support an infrastructure/flaky diagnosis. Compare with a baseline only when evidence points to a regression and the comparison can resolve a specific hypothesis; then report the result rather than changing the PR to chase unrelated failures.
8. **Stop when the proof is complete.** Do not broaden to workspace-wide tests without explicit user approval. Add a narrower consumer check only for a concrete coverage gap, dependency, or diagnosed failure. Record any remaining unverified surface explicitly.

## After semantic validation

For a large change, run `just fix -p <project>` only when no shared crate changed. If the change modifies a shared crate, run unscoped `just fix` as required by `AGENTS.md`. Always run `just fmt` after code edits. Run each required finalization step once, in the prescribed order. Mechanical changes do not justify repeating a completed semantic test loop. If finalization exposes or introduces a substantive code change, report that the final source revision is not covered by the earlier result and follow the repository's validation rules.

Keep PR CI evidence tied to its head SHA. A green run on an older commit does not validate a later push; a new commit should trigger CI for the new revision.
