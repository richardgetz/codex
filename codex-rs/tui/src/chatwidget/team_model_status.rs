//! Role-aware model and reasoning values for the footer status line.
//!
//! Team assignments are authoritative: Core canonicalizes every thread model
//! and effort patch to the active role's assigned profile. The current thread
//! settings are only a fallback when a role profile is missing from a snapshot.

use super::ChatWidget;
use codex_app_server_protocol::TeamMode;
use codex_app_server_protocol::TeamRole;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;

pub(super) enum TeamModelStatusItem {
    ModelName,
    ModelWithReasoning,
    Reasoning,
}

impl ChatWidget {
    pub(super) fn team_status_line_value(&self, item: TeamModelStatusItem) -> Option<String> {
        let team = self
            .team_settings
            .as_ref()
            .filter(|team| team.mode == TeamMode::LeadWorker)?;
        let active_role = team.role;
        let current_model = self.model_display_name().to_string();
        let current_reasoning = self.effective_reasoning_effort();
        let current_reasoning =
            Self::status_line_reasoning_effort_label(current_reasoning.as_ref());
        let profile_model_name = |model: Option<&str>| {
            model
                .filter(|model| !model.trim().is_empty())
                .map(|model| self.model_catalog.display_name(model).to_string())
        };
        let profile_reasoning = |effort: Option<&ReasoningEffortConfig>| {
            effort.map(|effort| Self::status_line_reasoning_effort_label(Some(effort)))
        };
        let lead_is_active = active_role == Some(TeamRole::Lead);
        let worker_is_active = active_role == Some(TeamRole::Worker);
        let lead_model = profile_model_name(team.lead_model.as_deref()).unwrap_or_else(|| {
            if lead_is_active {
                current_model.clone()
            } else {
                "not configured".to_string()
            }
        });
        let lead_reasoning =
            profile_reasoning(team.lead_reasoning_effort.as_ref()).unwrap_or_else(|| {
                if lead_is_active {
                    current_reasoning.clone()
                } else {
                    Self::status_line_reasoning_effort_label(None)
                }
            });
        let worker_model = profile_model_name(team.worker_model.as_deref()).unwrap_or_else(|| {
            if worker_is_active {
                current_model.clone()
            } else {
                "not configured".to_string()
            }
        });
        let worker_reasoning = profile_reasoning(team.worker_reasoning_effort.as_ref())
            .unwrap_or_else(|| {
                if worker_is_active {
                    current_reasoning.clone()
                } else {
                    Self::status_line_reasoning_effort_label(None)
                }
            });
        let lead_label = if lead_is_active {
            "Lead"
        } else {
            "Lead default"
        };
        let worker_label = if worker_is_active {
            "Worker"
        } else {
            "Worker default"
        };

        match item {
            TeamModelStatusItem::ModelName => Some(format!(
                "{lead_label}: {lead_model} · {worker_label}: {worker_model}"
            )),
            TeamModelStatusItem::Reasoning => Some(format!(
                "{lead_label}: {lead_reasoning} · {worker_label}: {worker_reasoning}"
            )),
            TeamModelStatusItem::ModelWithReasoning => {
                let active_profile_model = match active_role {
                    Some(TeamRole::Lead) => team.lead_model.as_deref(),
                    Some(TeamRole::Worker) => team.worker_model.as_deref(),
                    None => None,
                };
                let service_tier_label = if active_profile_model.is_none()
                    || active_profile_model == Some(self.current_model())
                {
                    self.current_service_tier()
                        .and_then(|service_tier| {
                            self.current_model_service_tier_commands()
                                .into_iter()
                                .find(|tier| tier.id == service_tier)
                                .map(|tier| tier.name)
                        })
                        .filter(|_| self.has_chatgpt_account)
                } else {
                    None
                };
                let mut lead_model = format!("{lead_model} {lead_reasoning}");
                let mut worker_model = format!("{worker_model} {worker_reasoning}");
                if let Some(service_tier_label) = service_tier_label {
                    match active_role {
                        Some(TeamRole::Lead) => {
                            lead_model.push(' ');
                            lead_model.push_str(&service_tier_label);
                        }
                        Some(TeamRole::Worker) => {
                            worker_model.push(' ');
                            worker_model.push_str(&service_tier_label);
                        }
                        None => {}
                    }
                }
                Some(format!(
                    "{lead_label}: {lead_model} · {worker_label}: {worker_model}"
                ))
            }
        }
    }
}
