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
    multi_agent_mode: Option<MultiAgentMode>,
    multi_agent_usage_hint_hash: Option<WorldStateHash>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct TeamPolicySnapshot {
    #[serde(default)]
    role: Option<TeamRole>,
    #[serde(default)]
    multi_agent_mode: Option<MultiAgentMode>,
    #[serde(default)]
    multi_agent_usage_hint_hash: Option<WorldStateHash>,
}

impl TeamPolicyState {
    pub(crate) fn new(role: TeamRole) -> Self {
        Self {
            role: Some(role),
            multi_agent_mode: None,
            multi_agent_usage_hint_hash: None,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            role: None,
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
}

impl WorldStateSection for TeamPolicyState {
    const ID: &'static str = "team_policy";
    type Snapshot = TeamPolicySnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        TeamPolicySnapshot {
            role: self.role,
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
                && previous.multi_agent_mode == self.multi_agent_mode
                && previous.multi_agent_usage_hint_hash == self.multi_agent_usage_hint_hash
        ) {
            return None;
        }
        match self.role {
            Some(role) => Some(Box::new(TeamInstructions::new(role))),
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
