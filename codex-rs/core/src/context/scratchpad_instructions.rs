use super::ContextualUserFragment;
use codex_config::types::ScratchpadFanoutConfig;

const SCRATCHPAD_INSTRUCTIONS_OPEN_TAG: &str = "<scratchpad_instructions>";
const SCRATCHPAD_INSTRUCTIONS_CLOSE_TAG: &str = "</scratchpad_instructions>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScratchpadInstructions {
    fanout: ScratchpadFanoutConfig,
}

impl ScratchpadInstructions {
    pub(crate) fn new(fanout: ScratchpadFanoutConfig) -> Self {
        Self { fanout }
    }
}

impl ContextualUserFragment for ScratchpadInstructions {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        (
            SCRATCHPAD_INSTRUCTIONS_OPEN_TAG,
            SCRATCHPAD_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            SCRATCHPAD_INSTRUCTIONS_OPEN_TAG,
            SCRATCHPAD_INSTRUCTIONS_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        let mut body = "\n## Built-in Scratchpad\n\
The built-in `scratchpad` tool namespace is available in this mode and is the canonical recovery ledger for non-trivial work.\n\
Use it proactively to keep durable working state across interruptions, compaction, waits, and delegation.\n\n\
Expected use:\n\
- Open or resume the scratchpad early for non-trivial tasks. If no explicit id is needed, `open_scratchpad` defaults to the current thread/session id; thread-owned scratchpads cannot be rebound or mutated from another thread.\n\
- For action-oriented tasks, write the initial task plan into `next_steps` before or as you start working so recovery has a concrete active queue. Keep `next_steps` limited to actionable work that still needs attention.\n\
- Whenever new tasks, issues, tests, review follow-ups, deployment checks, or other concrete work arise, add them to `next_steps` before they can be lost. Treat `next_steps` as short-term working memory for anything another agent would need after compaction.\n\
- As tasks finish, move the finished item out of active `next_steps` and record it in `completed` rather than dropping it from the ledger. Completed work belongs in `completed`; remaining work belongs in `next_steps`.\n\
- Keep `objective`, `status`, `completed`, `next_steps`, `pending_waits`, `blocked`, `run_policy`, `communication_policy`, `outcomes`, `delegations`, `resume_instructions`, `final_guard`, and recent notes current enough that another agent can recover the work. Use `update_scratchpad` to rename `objective` when the working goal changes.\n\
- Use `run_policy.continuous.enabled` as the durable continuous-run switch for the current thread. Move external waits to `pending_waits` and true blockers to `blocked`; use `wait_type = \"user_confirmation\"` for waits that need the user to confirm, grant access, merge something, or make a decision. Pending waits and blocked items alone are recovery context, not active work, so do not keep checking or keep the system awake when no actionable `next_steps` remain.\n\
- Use `communication_policy` for durable communication preferences. A communication channel failure alone should not be treated as permission to stop or fall back to final_response unless the main work is actually blocked.\n\
- Use `record_outcome` for measurable progress only when `[scratchpad].outcomes_enabled` is true. Include scope, metric/unit, baseline/current/delta, summary, tradeoffs, and commit/PR/artifact provenance when available; use `export_outcomes` or `/outcomes` when the user wants a portable postmortem.\n\
- Use `record_delegation` when handing scratchpad items to subagents. Include the subagent id/label, delegated item references, status, and child scratchpad id when available so parent-child lineage survives restarts.\n\
- Before waiting, delegating, ending a follow-up channel, merging, deploying, or stopping, update the scratchpad with the exact next recovery step.\n\
- When the user gives session-scoped safety rules for PR, merge, deploy, or AWS write actions, immediately persist them in `action_policy` with `set_action_policy` instead of only acknowledging them in prose. Examples include \"do not merge repo X\", \"only merge repo Y to staging/main\", or \"never deploy env prod\".\n\
- Before any PR, merge, deploy, release, benchmark launch, or AWS write action, call `check_action_allowed` with the relevant `action`, `repo`, `target_branch`, and/or `env`. If the action is denied, stop before taking the action and move the denial to `blocked` or `pending_waits`.\n\
- Use `set_action_policy`, `check_action_allowed`, and `mark_wait_checked` when the task has safety constraints or long-running waits.\n\
- Archive the scratchpad when the objective is finished; use `resume_scratchpad` or `lookup_scratchpads` when asked to recover older state.\n\n\
After context compaction, the harness may mechanically read the active thread scratchpad and loop a compact recovery summary back into the model when it still has recoverable work (`next_steps`, `pending_waits`, or `blocked`). Treat that recovery summary as authoritative working state and continue keeping the scratchpad updated.\n".to_string();
        if self.fanout.enabled {
            body.push_str(&format!(
                "\nScratchpad fanout is enabled with max_agents = {}. When `next_steps` contains several independent, disconnected tasks and subagents are permitted by the active instructions, you may delegate up to that many tasks in parallel. Keep the parent thread responsible for integration, review feedback, merge safety, and checking that child work does not violate prior user instructions. Do not fan out tightly coupled work or work whose next action blocks the parent.\n",
                self.fanout.max_agents
            ));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_mentions_core_tools() {
        let body = ScratchpadInstructions::new(ScratchpadFanoutConfig::default()).body();

        assert!(body.contains("open_scratchpad"));
        assert!(body.contains("mark_wait_checked"));
        assert!(body.contains("After context compaction"));
        assert!(body.contains("initial task plan into `next_steps`"));
        assert!(body.contains("new tasks, issues, tests, review follow-ups"));
        assert!(body.contains("record it in `completed`"));
    }

    #[test]
    fn body_mentions_fanout_when_enabled() {
        let body = ScratchpadInstructions::new(ScratchpadFanoutConfig {
            enabled: true,
            max_agents: 4,
        })
        .body();

        assert!(body.contains("Scratchpad fanout is enabled"));
        assert!(body.contains("max_agents = 4"));
    }
}
