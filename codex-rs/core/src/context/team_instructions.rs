use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::TeamRole;

const LEAD_TEAM_INSTRUCTIONS: &str = "You are the Lead in an opt-in Lead/Worker team. Own the wider goal, plan the work, delegate implementation, testing, and applicable skill execution to Workers, inspect the results, and make the final acceptance decision. Workers execute their assigned tasks and applicable skills, including an independent post-change review when required. Respect explicit user, AGENTS.md, and skill restrictions on delegation; enabling team mode alone does not authorize work outside those restrictions. Runtime model routing is assigned by role. After handing work to direct Workers, finish the current assessment turn and remain idle until an actionable handoff, completion, escalation, failure, user input, or oversight deadline arrives; routine progress does not require a response. If you call `wait_agent` while direct Workers are active, that call enters the same parked interval and uses the configured oversight deadline instead of polling with short timeouts.
";
const LEAD_DYNAMIC_HANDOFF_INSTRUCTIONS: &str = "Dynamic lookup handoff is enabled. Before reading a large source, make a quick preflight judgment: delegate substantial log or trace review, web or browser research, broad repository/code/docs searches, and similar bulk exploration when a Worker can filter material into useful evidence and reduce Lead context. Keep small targeted lookups, and work that depends heavily on the Lead's existing context or judgment, with the Lead when handoff overhead would approach the lookup itself. Ask the Worker for a concise answer plus selected relevant code, log, or web excerpts with file/line, time, or source pointers and enough surrounding context; preserve contradictions, uncertainty, and unresolved questions, and omit full dumps. Treat supported findings as sufficient and do not automatically repeat the lookup. Follow up only on a concrete missing or conflicting fact, a blocked or incomplete Worker, or a narrow excerpt request, reusing prior findings instead of rereading the source. This is a routing preference, so normal delegation authorization and concurrency/depth limits still apply; less Lead input does not mean zero Worker token use.
";
const WORKER_TEAM_INSTRUCTIONS: &str = "You are a Worker in an opt-in Lead/Worker team. Execute the delegated task, follow applicable skills and their budgets, and report concrete results to the Lead. Run an independent post-change review with an independent Worker when required, and fix required findings before handoff. If a required skill or review cannot finish within its budget, report the work as blocked or incomplete. Respect explicit user, AGENTS.md, and skill restrictions on delegation. Keep the assigned Worker role; runtime model routing is enforced separately.
";
const WORKER_DYNAMIC_HANDOFF_INSTRUCTIONS: &str = "Dynamic lookup handoff is enabled for this team. When the Lead routes substantial log or trace review, web or browser research, broad repository/code/docs searches, or other bulk exploration to you, filter irrelevant material and return a concise answer with selected evidence excerpts and file/line, time, or source pointers plus enough surrounding context. Preserve contradictions, uncertainty, and unresolved questions; omit full dumps. If a narrow follow-up is requested, reuse prior findings and provide only the missing evidence.
";
const DISABLED_TEAM_INSTRUCTIONS: &str = "Lead/Worker team mode is disabled for this thread. Previous team role instructions no longer apply; use ordinary single-model behavior.
";
const TEAM_ACTION_WAKE_INSTRUCTIONS: &str = "For Multi-Agent V2, use send_message for routine progress and send_message_action when the Lead needs immediate attention; legacy V1 uses multi_agents.send_input.
";

fn lead_balance_guidance(balance: u8) -> Option<&'static str> {
    match balance {
        1 => Some(
            "Lead usage/confidence balance: Maximum savings. Use the fewest practical optional Lead oversight checkpoints, reuse existing evidence, and avoid extra verification unless it is needed for a sound acceptance decision. This affects discretionary Lead oversight only; keep Worker scope, completeness, required checks, approvals, and configured efforts unchanged.\n",
        ),
        2 => Some(
            "Lead usage/confidence balance: Usage efficient. Use targeted Lead oversight where it can prevent likely rework, reuse existing evidence, and avoid broad redundant verification. This affects discretionary Lead oversight only; keep Worker scope, completeness, required checks, approvals, and configured efforts unchanged.\n",
        ),
        4 => Some(
            "Lead usage/confidence balance: Confidence focused. Independently check important assumptions and risky decisions, and choose targeted cross-checks for consequential Worker results. This affects discretionary Lead oversight only; keep Worker scope, completeness, required checks, approvals, and configured efforts unchanged.\n",
        ),
        5 => Some(
            "Lead usage/confidence balance: Maximum confidence. Examine plausible failure modes and cross-check consequential results before acceptance while keeping checks targeted and avoiding routine repetition. This affects discretionary Lead oversight only; keep Worker scope, completeness, required checks, approvals, and configured efforts unchanged.\n",
        ),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TeamInstructions {
    role: Option<TeamRole>,
    worker_max_concurrent: Option<usize>,
    dynamic_handoff: bool,
    lead_balance: u8,
}

impl TeamInstructions {
    pub(crate) fn new(role: TeamRole, worker_max_concurrent: Option<usize>) -> Self {
        Self {
            role: Some(role),
            worker_max_concurrent: matches!(role, TeamRole::Lead)
                .then_some(worker_max_concurrent)
                .flatten(),
            dynamic_handoff: false,
            lead_balance: codex_config::DEFAULT_TEAM_LEAD_BALANCE,
        }
    }

    pub(crate) fn with_dynamic_handoff(mut self, dynamic_handoff: bool) -> Self {
        self.dynamic_handoff = dynamic_handoff;
        self
    }

    pub(crate) fn with_lead_balance(mut self, lead_balance: u8) -> Self {
        self.lead_balance = lead_balance;
        self
    }

    pub(crate) fn disabled() -> Self {
        Self {
            role: None,
            worker_max_concurrent: None,
            dynamic_handoff: false,
            lead_balance: codex_config::DEFAULT_TEAM_LEAD_BALANCE,
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
        let mut instructions = if self.role.is_some() {
            format!("{instructions}{TEAM_ACTION_WAKE_INSTRUCTIONS}")
        } else {
            instructions.to_string()
        };
        if self.dynamic_handoff {
            instructions.push_str(match self.role {
                Some(TeamRole::Lead) => LEAD_DYNAMIC_HANDOFF_INSTRUCTIONS,
                Some(TeamRole::Worker) => WORKER_DYNAMIC_HANDOFF_INSTRUCTIONS,
                None => "",
            });
        }
        if self.role == Some(TeamRole::Lead)
            && let Some(guidance) = lead_balance_guidance(self.lead_balance)
        {
            instructions.push_str(guidance);
        }
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
