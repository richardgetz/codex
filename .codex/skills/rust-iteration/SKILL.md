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
| “Does this behavior work and compile its affected consumer?” | Run `just test -p <crate> <test-filter>` on the narrowest relevant target. Use the package/target selector to control build scope; the filter only narrows tests. This compiles the selected target and runs the test; do not precede it with a separate check unless that target omits an affected consumer. |
| “Does an affected Rust production target compile, with no relevant behavior test?” | Use `cargo check -p <crate>` for the narrowest affected consumer; use `just codex` when the CLI and code-mode host are the intended consumers. Include downstream consumers only when the change reaches them. |
| “Did an API/schema change regenerate correctly?” | Run the relevant generator once after the source shape is settled, inspect the generated diff, then validate its narrow consumer. |
| “Is the final patch formatted/lint-clean?” | For a large Rust change, run `just fix -p <project>` when no shared crate changed; if a shared crate changed, use unscoped `just fix` as required by `AGENTS.md`. Then run required `just fmt` after code edits. |

Do not substitute one kind of proof for another. A successful app build does not establish test-only behavior, and a passing focused test does not prove a separate binary consumer compiles when it is outside that test target.

## One focused iteration

1. **Reuse evidence first.** Check whether this exact source/config revision already passed the same target and scope. Reuse that result; do not rerun it to get a fresher timestamp.
2. **Do a short source and fixture preflight before the first Rust run.** Trace the changed production path and the relevant async test from submitted operation through scripted response/event to the observed state or wake. Check only the helpers, waits, consuming receivers, and timing bounds that form that test's proof; confirm the fixture can reach the expected terminal condition and the selected target compiles the affected consumer. Correct a known fixture mismatch before compiling. Do not use repeated builds to discover an unreachable test timeline or inspect unrelated async waits.
3. **Batch edits before compiling.** Finish related implementation, test, fixture, and call-site edits; inspect the complete diff and relevant compiler-sensitive call sites once before starting Rust. Do not use repeated builds as a substitute for reading diagnostics or reviewing the changed code.
4. **Assign one build owner.** Integrate intended edits before building, use the repository's shared Cargo target/cache, serialize Rust commands, and freeze the source while validation is running. Do not launch a duplicate command because output is quiet or a lock is taking time.
5. **Keep a run record and report terminal events immediately.** Before launch, capture the integrated SHA, exact command and package/target/filter, build owner, target/cache location, and session or log handle. Retain and wait on that same command session. As soon as it terminates, report the SHA, command, session/log, exit status, first meaningful failure (or pass scope), interpretation, and next action to the owner/Lead. Do not leave a terminal result unreported while continuing other work.
6. **Triage before editing.** Classify the result as production compilation, `cfg(test)` compilation, a test assertion/runtime failure, generated-fixture drift, or infrastructure. Start from the earliest actionable diagnostic rather than cascaded errors, and verify which code path the selected target compiled.
7. **Repair once, then rerun once for the new revision.** Group all known related compiler/test fixes into one patch, review it, record the new SHA, and repeat the narrow proof. Retry the same command only when logs support an infrastructure/flaky diagnosis. Compare with a baseline only when evidence points to a regression and the comparison can resolve a specific hypothesis; then report the result rather than changing the PR to chase unrelated failures.
8. **Stop when the proof is complete.** Do not broaden to workspace-wide tests without explicit user approval. Add a narrower consumer check only for a concrete coverage gap, dependency, or diagnosed failure. Record any remaining unverified surface explicitly.

## After semantic validation

After the narrow semantic test passes, run the required mechanical checks in the
scope and order required by `AGENTS.md`: for a large change, use `just fix
-p <project>` when no shared crate changed or unscoped `just fix` when it did;
run required `just fmt` after code edits. Inspect the resulting diff, freeze that
integrated revision, and run one independent post-change review over the changed
diff. A source/fixture feasibility review before the test is useful when needed,
but it is not a reason to repeat the post-change review.

If review finds a concrete issue, fix only the affected changed scope and rerun
the narrow validation that covers that fix; do not repeat unaffected tests or
restart a broad review cascade for unchanged paths. Run required mechanical
checks for the repaired source, then do a focused review follow-up when the
applicable review policy requires one. Record the revision and evidence for
each run; stop when the changed behavior is covered and review findings are
resolved. Mechanical-only changes do not require repeating a completed
semantic test. If finalization exposes or introduces a substantive code change,
report that the final source revision is not covered by earlier results and
follow the repository's validation rules.

Keep PR CI evidence tied to its head SHA. A green run on an older commit does not validate a later push; a new commit should trigger CI for the new revision.
