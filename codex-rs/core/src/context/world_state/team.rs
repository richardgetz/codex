use super::MultiAgentModeState;
use super::PreviousSectionState;
use super::WorldStateHash;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::TeamInstructions;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::protocol::TeamRole;
use serde::Deserialize;
use serde::Serialize;

/// Model-visible responsibilities for the effective Lead/Worker assignment.
#[derive(Clone, Debug)]
pub(crate) struct TeamPolicyState {
    role: Option<TeamRole>,
    worker_max_concurrent: Option<usize>,
    dynamic_handoff: bool,
    multi_agent_mode: Option<MultiAgentMode>,
    multi_agent_usage_hint_hash: Option<WorldStateHash>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct TeamPolicySnapshot {
    #[serde(default)]
    role: Option<TeamRole>,
    #[serde(default)]
    worker_max_concurrent: Option<usize>,
    #[serde(default)]
    dynamic_handoff: bool,
    #[serde(default)]
    multi_agent_mode: Option<MultiAgentMode>,
    #[serde(default)]
    multi_agent_usage_hint_hash: Option<WorldStateHash>,
}

impl TeamPolicyState {
    pub(crate) fn new(role: TeamRole, worker_max_concurrent: Option<usize>) -> Self {
        Self {
            role: Some(role),
            worker_max_concurrent: matches!(role, TeamRole::Lead)
                .then_some(worker_max_concurrent)
                .flatten(),
            dynamic_handoff: false,
            multi_agent_mode: None,
            multi_agent_usage_hint_hash: None,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            role: None,
            worker_max_concurrent: None,
            dynamic_handoff: false,
            multi_agent_mode: None,
            multi_agent_usage_hint_hash: None,
        }
    }

    pub(crate) fn with_multi_agent_mode(mut self, state: &MultiAgentModeState) -> Self {
        let (mode, usage_hint_hash) = state.team_policy_dependency();
        self.multi_agent_mode = mode;
        self.multi_agent_usage_hint_hash = usage_hint_hash;
        self
    }

    pub(crate) fn with_dynamic_handoff(mut self, dynamic_handoff: bool) -> Self {
        self.dynamic_handoff = dynamic_handoff;
        self
    }
}

impl WorldStateSection for TeamPolicyState {
    const ID: &'static str = "team_policy";
    type Snapshot = TeamPolicySnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        TeamPolicySnapshot {
            role: self.role,
            worker_max_concurrent: self.worker_max_concurrent,
            dynamic_handoff: self.dynamic_handoff,
            multi_agent_mode: self.multi_agent_mode.clone(),
            multi_agent_usage_hint_hash: self.multi_agent_usage_hint_hash.clone(),
        }
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && TeamInstructions::matches_text(text)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        if matches!(previous, PreviousSectionState::Known(previous)
            if previous.role == self.role
                && previous.worker_max_concurrent == self.worker_max_concurrent
                && previous.dynamic_handoff == self.dynamic_handoff
                && previous.multi_agent_mode == self.multi_agent_mode
                && previous.multi_agent_usage_hint_hash == self.multi_agent_usage_hint_hash
        ) {
            return None;
        }
        match self.role {
            Some(role) => Some(Box::new(
                TeamInstructions::new(role, self.worker_max_concurrent)
                    .with_dynamic_handoff(self.dynamic_handoff),
            )),
            None if matches!(
                previous,
                PreviousSectionState::Known(previous) if previous.role.is_some()
            ) || matches!(previous, PreviousSectionState::Unknown) =>
            {
                Some(Box::new(TeamInstructions::disabled()))
            }
            None => None,
        }
    }
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
