use super::ContextualUserFragment;

const SCRATCHPAD_INSTRUCTIONS_OPEN_TAG: &str = "<scratchpad_instructions>";
const SCRATCHPAD_INSTRUCTIONS_CLOSE_TAG: &str = "</scratchpad_instructions>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScratchpadInstructions;

impl ScratchpadInstructions {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl ContextualUserFragment for ScratchpadInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = SCRATCHPAD_INSTRUCTIONS_OPEN_TAG;
    const END_MARKER: &'static str = SCRATCHPAD_INSTRUCTIONS_CLOSE_TAG;

    fn body(&self) -> String {
        "\n## Built-in Scratchpad\n\
The built-in `scratchpad` tool namespace is available in this mode and is the canonical recovery ledger for non-trivial work.\n\
Use it proactively to keep durable working state across interruptions, compaction, waits, and delegation.\n\n\
Expected use:\n\
- Open or resume the scratchpad early for non-trivial tasks. If no explicit id is needed, `open_scratchpad` defaults to the current thread/session id; thread-owned scratchpads cannot be rebound or mutated from another thread.\n\
- Keep `objective`, `status`, `completed`, `next_steps`, `pending_waits`, `run_policy`, `communication_policy`, `outcomes`, `delegations`, `resume_instructions`, `final_guard`, and recent notes current enough that another agent can recover the work.\n\
- Use `run_policy.continuous.enabled` as the durable continuous-run switch for the current thread. When enabled and unfinished `next_steps` or `pending_waits` remain, the runtime will loop back instead of allowing finalization.\n\
- Use `communication_policy` for durable communication preferences. A communication channel failure alone should not be treated as permission to stop or fall back to final_response unless the main work is actually blocked.\n\
- Use `record_outcome` for measurable progress only when `[scratchpad].outcomes_enabled` is true. Include scope, metric/unit, baseline/current/delta, summary, tradeoffs, and commit/PR/artifact provenance when available; use `export_outcomes` or `/outcomes` when the user wants a portable postmortem.\n\
- Use `record_delegation` when handing scratchpad items to subagents. Include the subagent id/label, delegated item references, status, and child scratchpad id when available so parent-child lineage survives restarts.\n\
- Before waiting, delegating, ending a follow-up channel, merging, deploying, or stopping, update the scratchpad with the exact next recovery step.\n\
- Use `set_action_policy`, `check_action_allowed`, and `mark_wait_checked` when the task has safety constraints or long-running waits.\n\
- Archive the scratchpad when the objective is finished; use `resume_scratchpad` or `lookup_scratchpads` when asked to recover older state.\n\n\
After context compaction, the harness may mechanically read the active thread scratchpad and loop a compact recovery summary back into the model when it still has uncompleted work (`next_steps` or `pending_waits`). Treat that recovery summary as authoritative working state and continue keeping the scratchpad updated.\n".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_mentions_core_tools() {
        let body = ScratchpadInstructions::new().body();

        assert!(body.contains("open_scratchpad"));
        assert!(body.contains("mark_wait_checked"));
        assert!(body.contains("After context compaction"));
    }
}
