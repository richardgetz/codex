use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::TeamRole;

const LEAD_TEAM_INSTRUCTIONS: &str = "You are the Lead in an opt-in Lead/Worker team. Own the wider goal, plan the work, delegate implementation, testing, and applicable skill execution to Workers, inspect the results, and make the final acceptance decision. Workers execute their assigned tasks and applicable skills, including an independent post-change review when required. Respect explicit user, AGENTS.md, and skill restrictions on delegation; enabling team mode alone does not authorize work outside those restrictions. Runtime model routing is assigned by role. After handing work to direct Workers, finish the current assessment turn and remain idle until an actionable handoff, completion, escalation, failure, user input, or oversight deadline arrives; routine progress does not require a response. If you call `wait_agent` while direct Workers are active, that call enters the same parked interval and uses the configured oversight deadline instead of polling with short timeouts.
";
const WORKER_TEAM_INSTRUCTIONS: &str = "You are a Worker in an opt-in Lead/Worker team. Execute the delegated task, follow applicable skills and their budgets, and report concrete results to the Lead. Run an independent post-change review with an independent Worker when required, and fix required findings before handoff. If a required skill or review cannot finish within its budget, report the work as blocked or incomplete. Respect explicit user, AGENTS.md, and skill restrictions on delegation. Keep the assigned Worker role; runtime model routing is enforced separately.
";
const DISABLED_TEAM_INSTRUCTIONS: &str = "Lead/Worker team mode is disabled for this thread. Previous team role instructions no longer apply; use ordinary single-model behavior.
";
const TEAM_ACTION_WAKE_INSTRUCTIONS: &str = "For Multi-Agent V2, use send_message for routine progress and send_message_action when the Lead needs immediate attention; legacy V1 uses multi_agents.send_input.
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TeamInstructions {
    role: Option<TeamRole>,
    worker_max_concurrent: Option<usize>,
}

impl TeamInstructions {
    pub(crate) fn new(role: TeamRole, worker_max_concurrent: Option<usize>) -> Self {
        Self {
            role: Some(role),
            worker_max_concurrent: matches!(role, TeamRole::Lead)
                .then_some(worker_max_concurrent)
                .flatten(),
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            role: None,
            worker_max_concurrent: None,
        }
    }
}

impl ContextualUserFragment for TeamInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("team.role_instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn requires_separate_message(&self) -> bool {
        true
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<team_role_instructions>", "</team_role_instructions>")
    }

    fn body(&self) -> String {
        let instructions = match self.role {
            Some(TeamRole::Lead) => LEAD_TEAM_INSTRUCTIONS,
            Some(TeamRole::Worker) => WORKER_TEAM_INSTRUCTIONS,
            None => DISABLED_TEAM_INSTRUCTIONS,
        };
        let instructions = if self.role.is_some() {
            format!("{instructions}{TEAM_ACTION_WAKE_INSTRUCTIONS}")
        } else {
            instructions.to_string()
        };
        let Some(worker_max_concurrent) = self.worker_max_concurrent else {
            return instructions;
        };
        format!(
            "{instructions}\nDirect Worker concurrency ceiling: {worker_max_concurrent} concurrently active Workers. This is a ceiling, not a target; choose practical parallelism that balances useful progress with coordination overhead. Grandchildren are excluded from this ceiling. Existing global agent-count, depth, and resource limits still apply.\n"
        )
    }
}

#[cfg(test)]
#[path = "team_instructions_tests.rs"]
mod tests;
