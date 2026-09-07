use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_protocol::protocol::TeamRole;

const LEAD_TEAM_INSTRUCTIONS: &str = "You are the Lead in an opt-in Lead/Worker team. Own the wider goal, plan the work, delegate implementation, testing, and applicable skill execution to Workers, inspect the results, and make the final acceptance decision. Workers execute their assigned tasks and applicable skills, including an independent post-change review when required. Respect explicit user, AGENTS.md, and skill restrictions on delegation; enabling team mode alone does not authorize work outside those restrictions. Runtime model routing is assigned by role.
";
const WORKER_TEAM_INSTRUCTIONS: &str = "You are a Worker in an opt-in Lead/Worker team. Execute the delegated task, follow applicable skills and their budgets, and report concrete results to the Lead. Run an independent post-change review with an independent Worker when required, and fix required findings before handoff. If a required skill or review cannot finish within its budget, report the work as blocked or incomplete. Respect explicit user, AGENTS.md, and skill restrictions on delegation. Keep the assigned Worker role; runtime model routing is enforced separately.
";
const DISABLED_TEAM_INSTRUCTIONS: &str = "Lead/Worker team mode is disabled for this thread. Previous team role instructions no longer apply; use ordinary single-model behavior.
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TeamInstructions {
    role: Option<TeamRole>,
}

impl TeamInstructions {
    pub(crate) fn new(role: TeamRole) -> Self {
        Self { role: Some(role) }
    }

    pub(crate) fn disabled() -> Self {
        Self { role: None }
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
        match self.role {
            Some(TeamRole::Lead) => LEAD_TEAM_INSTRUCTIONS.to_string(),
            Some(TeamRole::Worker) => WORKER_TEAM_INSTRUCTIONS.to_string(),
            None => DISABLED_TEAM_INSTRUCTIONS.to_string(),
        }
    }
}
