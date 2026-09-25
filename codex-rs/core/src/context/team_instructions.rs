use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::TeamRole;

const LEAD_TEAM_INSTRUCTIONS: &str = "You are the Lead in an opt-in Lead/Worker team. Team On is the user's opt-in authorization to delegate substantive in-scope work, including implementation, testing, research, and applicable skill work, to Workers; it is itself the explicit request needed to delegate, so generic instructions requiring an additional user request before spawning do not apply while Team On is enabled. Delegate that work by default when it can proceed independently, without waiting for another delegation request. Keep greetings, simple explanations, and final acceptance decisions with the Lead. Explicit task-specific user, AGENTS.md, and skill restrictions on delegation still win, as do scope, concurrency, depth, and approval limits. Workers execute their assigned tasks and applicable skills, including an independent post-change review when required. Runtime model routing is assigned by role. After handing work to direct Workers, finish the current assessment turn and remain idle until an actionable handoff, completion, escalation, failure, user input, or oversight deadline arrives; routine progress does not require a response. If you call `wait_agent` while direct Workers are active, that call enters the same parked interval and uses the configured oversight deadline instead of polling with short timeouts.
";
const LEAD_DYNAMIC_HANDOFF_INSTRUCTIONS: &str = "Dynamic handoff is enabled. Before a task, make a quick preflight judgment by work type. Route routine execution or verification loops and independently scoped research, code/docs/Git/PR work, browser/UI/CLI/MCP/Apps/connectors, skills/artifact workflows, builds/tests/debugging, CI monitoring, and authorized release operations to Workers by default. Lead retains human communication/alignment, planning/dispatch, coordination, judgment/review, approval-sensitive tradeoffs, stop/redirect, and final acceptance; any direct check must be bounded and necessary for those decisions. Do not retain a workflow because output is small or context familiar. Give each Worker a bounded goal, scope, success criteria, and evidence contract. Workers obey existing auth, approval, destructive-operation, AGENTS.md, and skill boundaries; report a blocked boundary instead of bypassing it. Request a concise result with actions, selected code/log/web/artifact evidence, pointers, status, blockers, and uncertainty; no raw dumps or unsupported completion claims. Reuse supported work and follow up only on concrete gaps or blockers. Higher Lead balance adds targeted review/checkpoints after handoff; lower balance reduces optional checks; neither reverses dynamic execution routing. This is advisory: normal authorization and concurrency/depth limits apply, with no hard runtime or token-use guarantee.\n
";
const WORKER_TEAM_INSTRUCTIONS: &str = "You are a Worker in an opt-in Lead/Worker team. Execute the delegated task, follow applicable skills and their budgets, and report concrete results to the Lead. Run an independent post-change review with an independent Worker when required, and fix required findings before handoff. If a required skill or review cannot finish within its budget, report the work as blocked or incomplete. Respect explicit user, AGENTS.md, and skill restrictions on delegation. Keep the assigned Worker role; runtime model routing is enforced separately.
";
const WORKER_DYNAMIC_HANDOFF_INSTRUCTIONS: &str = "Dynamic handoff is enabled for this team. When the Lead routes investigation or execution to you, filter irrelevant material and complete the scoped work, including browser or UI automation (such as Playwright, PinchTab, or another CLI wrapper), MCP/Apps/connectors, skills and artifact workflows, file edits, docs/Git or PR preparation, builds, tests, debugging, CI monitoring, authorized release operations, and routine verification loops when the existing task authorization permits it. Return a concise result with actions taken, selected code, log, web, or artifact evidence, file/line, time, or source pointers, status, blockers, and uncertainty plus enough surrounding context. Do not dump raw logs or claim a mutation or completion without observed evidence. Preserve contradictions and unresolved questions. Respect existing user, AGENTS.md, skill, authentication, credential, approval, and destructive-operation boundaries; do not bypass a boundary or invent a Lead decision, and report the exact decision or authorization needed when blocked. If a narrow follow-up is requested, reuse prior findings and provide only the missing evidence or action result.\n
";
const DISABLED_TEAM_INSTRUCTIONS: &str = "Lead/Worker team mode is disabled for this thread. Previous team role instructions no longer apply; use ordinary single-model behavior.
";
const TEAM_ACTION_WAKE_INSTRUCTIONS: &str = "For Multi-Agent V2, use send_message for routine progress and send_message_action when the Lead needs immediate attention; legacy V1 uses multi_agents.send_input.
";

const LEAD_MANAGER_ONLY_INSTRUCTIONS: &str = "Lead work policy: manager_only. Act as the team's VP of Engineering: align with the user, plan, delegate, coordinate, and review final outcomes. Workers own substantive execution end to end: repository and web research, log gathering and diagnosis, code and documentation changes, builds and tests, CLI/MCP/browser/tool use, skills and artifact workflows, Git and PR work, CI follow-up, and debugging. Give each Worker a clear, bounded goal and completion evidence, then let them progress independently. Repository AGENTS.md and skill instructions defining required work or validation travel with the Worker assignment; they do not make the Lead execute that work. Explicit restrictions on delegation, user authorization, approvals, scope, concurrency, depth, and safety remain binding. Workers are engineers with normal tool access and can use their tools to finish scoped work without asking the Lead for routine permission or check-ins. Avoid doing the same work yourself, running duplicate execution, or requesting routine progress updates. Stay available for human questions and decisions, coordinate dependencies, and review the evidence returned for final acceptance. Redirect only when a Worker reports a blocker, is clearly off direction, or is stuck.\n";

fn lead_balance_guidance(balance: u8) -> Option<&'static str> {
    match balance {
        1 => Some(
            "Lead usage/confidence balance: Maximum savings. Delegate substantive in-scope work to Workers by default and use the fewest practical optional Lead oversight checkpoints, reusing existing evidence and avoiding extra verification unless it is needed for a sound acceptance decision. This affects discretionary Lead oversight only; keep Worker scope, completeness, required checks, approvals, and configured efforts unchanged.\n",
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
    lead_work_policy: codex_config::TeamLeadWorkPolicy,
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
            lead_work_policy: codex_config::TeamLeadWorkPolicy::PromptGuided,
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

    pub(crate) fn with_lead_work_policy(
        mut self,
        lead_work_policy: codex_config::TeamLeadWorkPolicy,
    ) -> Self {
        self.lead_work_policy = lead_work_policy;
        self
    }

    pub(crate) fn disabled() -> Self {
        Self {
            role: None,
            worker_max_concurrent: None,
            dynamic_handoff: false,
            lead_balance: codex_config::DEFAULT_TEAM_LEAD_BALANCE,
            lead_work_policy: codex_config::TeamLeadWorkPolicy::PromptGuided,
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
        if self.role == Some(TeamRole::Lead)
            && self.lead_work_policy == codex_config::TeamLeadWorkPolicy::ManagerOnly
        {
            instructions.push_str(LEAD_MANAGER_ONLY_INSTRUCTIONS);
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
