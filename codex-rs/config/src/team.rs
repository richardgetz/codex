use codex_protocol::openai_models::ReasoningEffort;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Default maximum idle interval before a Lead receives an oversight wake.
pub const DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES: u64 = 30;
/// Whether Lead lookup work is delegated to Workers by default.
pub const DEFAULT_TEAM_LEAD_DYNAMIC_HANDOFF: bool = false;
/// Largest supported Lead oversight interval. Tokio timers represent far-future
/// instants only within roughly thirty years, so this bound keeps all duration
/// and Unix timestamp arithmetic representable while still allowing long work.
pub const MAX_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES: u64 = 30 * 365 * 24 * 60;

/// The two model assignments used by an opt-in Lead/Worker session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamConfig {
    /// Whether a newly created session starts with the team assignments active.
    pub enabled: bool,
    /// The configured Lead and Worker assignments, when both are present.
    pub profiles: Option<TeamModelProfiles>,
    /// Maximum number of concurrently active direct Workers for a Lead session.
    pub worker_max_concurrent: Option<usize>,
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
    pub worker: Option<TeamWorkerProfileToml>,
}

/// A model and reasoning effort read from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct TeamModelProfileToml {
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Whether the Lead should make a quick preflight and delegate substantial lookup work to a
    /// Worker when filtering bulk material can reduce Lead context.
    pub dynamic_handoff: Option<bool>,
    /// Minutes a Lead may remain idle while direct Workers are active before
    /// an oversight wake is emitted. A missing value uses the 30-minute default.
    #[schemars(range(min = 1, max = 15768000))]
    pub oversight_timeout_minutes: Option<u64>,
}

/// A Worker model and optional direct-child concurrency ceiling read from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct TeamWorkerProfileToml {
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Maximum number of concurrently active direct Workers for a Lead session.
    #[schemars(range(min = 1))]
    pub max_concurrent: Option<usize>,
}

impl TryFrom<TeamToml> for TeamConfig {
    type Error = String;

    fn try_from(value: TeamToml) -> Result<Self, Self::Error> {
        let TeamToml {
            enabled,
            lead,
            worker,
        } = value;
        if worker
            .as_ref()
            .and_then(|worker| worker.max_concurrent)
            .is_some_and(|max_concurrent| max_concurrent == 0)
        {
            return Err("team.worker.max_concurrent must be at least 1".into());
        }
        let worker_max_concurrent = worker.as_ref().and_then(|worker| worker.max_concurrent);
        let profiles = match (lead, worker) {
            (None, None) => None,
            (Some(lead), Some(worker)) => {
                let dynamic_handoff = lead
                    .dynamic_handoff
                    .unwrap_or(DEFAULT_TEAM_LEAD_DYNAMIC_HANDOFF);
                let oversight_timeout_minutes = lead
                    .oversight_timeout_minutes
                    .unwrap_or(DEFAULT_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES);
                if oversight_timeout_minutes == 0 {
                    return Err("team.lead.oversight_timeout_minutes must be at least 1".into());
                }
                if oversight_timeout_minutes > MAX_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES {
                    return Err(format!(
                        "team.lead.oversight_timeout_minutes must be at most {MAX_TEAM_LEAD_OVERSIGHT_TIMEOUT_MINUTES}"
                    ));
                }
                Some(TeamModelProfiles {
                    lead: TeamModelProfile::try_from(("lead", lead))?,
                    worker: TeamModelProfile::try_from(("worker", worker))?,
                    lead_dynamic_handoff: dynamic_handoff,
                    lead_oversight_timeout_minutes: oversight_timeout_minutes,
                })
            }
            (Some(_), None) => {
                return Err("team.worker must be configured when team.lead is configured".into());
            }
            (None, Some(_)) => {
                return Err("team.lead must be configured when team.worker is configured".into());
            }
        };
        let enabled = enabled.unwrap_or(false);
        if enabled && profiles.is_none() {
            return Err("team.enabled requires both team.lead and team.worker profiles".into());
        }
        Ok(Self {
            enabled,
            profiles,
            worker_max_concurrent,
        })
    }
}

/// The resolved pair of model assignments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamModelProfiles {
    pub lead: TeamModelProfile,
    pub worker: TeamModelProfile,
    /// Whether the Lead should preflight and delegate substantial lookup work to Workers.
    pub lead_dynamic_handoff: bool,
    /// Effective Lead oversight interval in minutes.
    pub lead_oversight_timeout_minutes: u64,
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

impl TryFrom<(&str, TeamWorkerProfileToml)> for TeamModelProfile {
    type Error = String;

    fn try_from((role, value): (&str, TeamWorkerProfileToml)) -> Result<Self, Self::Error> {
        let TeamWorkerProfileToml {
            model,
            reasoning_effort,
            max_concurrent: _,
        } = value;
        TeamModelProfile::try_from((
            role,
            TeamModelProfileToml {
                model,
                reasoning_effort,
                dynamic_handoff: None,
                oversight_timeout_minutes: None,
            },
        ))
    }
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
