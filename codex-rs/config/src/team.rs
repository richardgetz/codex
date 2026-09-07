use codex_protocol::openai_models::ReasoningEffort;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// The two model assignments used by an opt-in Lead/Worker session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamConfig {
    /// Whether a newly created session starts with the team assignments active.
    pub enabled: bool,
    /// The configured Lead and Worker assignments, when both are present.
    pub profiles: Option<TeamModelProfiles>,
}

/// The only roles recognized by the v1 team model policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamRole {
    Lead,
    Worker,
}

/// A concrete model and effort assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamModelProfile {
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
}

/// Effective `[team]` configuration.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TeamToml {
    /// Whether new sessions should start with Lead/Worker assignments enabled.
    pub enabled: Option<bool>,
    /// Model assignment for the root coordinator and acceptance agent.
    pub lead: Option<TeamModelProfileToml>,
    /// Model assignment for delegated work and independent reviewers.
    pub worker: Option<TeamModelProfileToml>,
}

/// A model and reasoning effort read from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TeamModelProfileToml {
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl TryFrom<TeamToml> for TeamConfig {
    type Error = String;

    fn try_from(value: TeamToml) -> Result<Self, Self::Error> {
        let profiles = match (value.lead, value.worker) {
            (None, None) => None,
            (Some(lead), Some(worker)) => Some(TeamModelProfiles {
                lead: TeamModelProfile::try_from(("lead", lead))?,
                worker: TeamModelProfile::try_from(("worker", worker))?,
            }),
            (Some(_), None) => {
                return Err("team.worker must be configured when team.lead is configured".into());
            }
            (None, Some(_)) => {
                return Err("team.lead must be configured when team.worker is configured".into());
            }
        };
        let enabled = value.enabled.unwrap_or(false);
        if enabled && profiles.is_none() {
            return Err("team.enabled requires both team.lead and team.worker profiles".into());
        }
        Ok(Self { enabled, profiles })
    }
}

/// The resolved pair of model assignments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamModelProfiles {
    pub lead: TeamModelProfile,
    pub worker: TeamModelProfile,
}

impl TryFrom<(&str, TeamModelProfileToml)> for TeamModelProfile {
    type Error = String;

    fn try_from((role, value): (&str, TeamModelProfileToml)) -> Result<Self, Self::Error> {
        let model = value
            .model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty())
            .ok_or_else(|| format!("team.{role}.model must be a non-empty string"))?;
        let reasoning_effort = value
            .reasoning_effort
            .ok_or_else(|| format!("team.{role}.reasoning_effort must be configured"))?;
        Ok(Self {
            model,
            reasoning_effort,
        })
    }
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
