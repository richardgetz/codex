//! Slash-command parsing and authoritative status rendering for Lead/Worker teams.

use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use codex_app_server_protocol::TeamMode;
use codex_app_server_protocol::TeamRole;
use codex_app_server_protocol::ThreadTeamSettings;

pub(crate) const TEAM_USAGE: &str =
    "Usage: /team [on|off|status|balance [1..5]|lead [<model> <effort>]|worker [<model> <effort>]]";

const LEAD_BALANCE_OPTIONS: [(u8, &str, &str); 5] = [
    (
        1,
        "Maximum savings",
        "Use the fewest practical optional Lead oversight checkpoints and reuse existing evidence.",
    ),
    (
        2,
        "Usage efficient",
        "Use targeted Lead oversight where it can prevent likely rework.",
    ),
    (
        3,
        "Current behavior (default)",
        "Keep today's discretionary Lead oversight behavior unchanged.",
    ),
    (
        4,
        "Confidence focused",
        "Independently check important assumptions and risky decisions.",
    ),
    (
        5,
        "Maximum confidence",
        "Examine plausible failure modes and cross-check consequential results.",
    ),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TeamCommand {
    On,
    Off,
    Status,
    SelectProfile {
        role: TeamRole,
    },
    ConfigureProfile {
        role: TeamRole,
        model: String,
        effort: codex_protocol::openai_models::ReasoningEffort,
    },
    SelectBalance,
    ConfigureBalance {
        balance: u8,
    },
}

pub(crate) fn parse_team_command(args: &str) -> Result<TeamCommand, &'static str> {
    let mut parts = args.split_whitespace();
    let command = parts.next().unwrap_or_default();
    match command.to_ascii_lowercase().as_str() {
        "" | "status" if parts.next().is_none() => Ok(TeamCommand::Status),
        "on" if parts.next().is_none() => Ok(TeamCommand::On),
        "off" if parts.next().is_none() => Ok(TeamCommand::Off),
        "balance" => match parts.next() {
            None => Ok(TeamCommand::SelectBalance),
            Some(balance) if parts.next().is_none() => balance
                .parse::<u8>()
                .ok()
                .filter(|balance| (1..=5).contains(balance))
                .map(|balance| TeamCommand::ConfigureBalance { balance })
                .ok_or(TEAM_USAGE),
            Some(_) => Err(TEAM_USAGE),
        },
        "lead" | "worker" => {
            let role = if command.eq_ignore_ascii_case("lead") {
                TeamRole::Lead
            } else {
                TeamRole::Worker
            };
            let Some(model) = parts.next() else {
                return parts
                    .next()
                    .is_none()
                    .then_some(TeamCommand::SelectProfile { role })
                    .ok_or(TEAM_USAGE);
            };
            let Some(effort) = parts.next() else {
                return Err(TEAM_USAGE);
            };
            if parts.next().is_some() || model.trim().is_empty() {
                return Err(TEAM_USAGE);
            }
            let effort = effort.parse().map_err(|_| TEAM_USAGE)?;
            Ok(TeamCommand::ConfigureProfile {
                role,
                model: model.to_string(),
                effort,
            })
        }
        _ => Err(TEAM_USAGE),
    }
}

impl ChatWidget {
    pub(crate) fn validate_team_profile_command(
        &self,
        role: TeamRole,
        model: &str,
        effort: &codex_protocol::openai_models::ReasoningEffort,
    ) -> Result<(), String> {
        let Some(preset) = self
            .model_catalog
            .try_list_models()
            .map_err(|_| "model catalog is unavailable; try again shortly".to_string())?
            .into_iter()
            .find(|preset| preset.model == model)
        else {
            return Err(format!(
                "{} model `{model}` is not available in the current model catalog",
                role_label(role)
            ));
        };
        if preset.model.starts_with("codex-auto-") {
            return Err(format!(
                "{} profiles require a concrete model; `{model}` is an automatic model",
                role_label(role)
            ));
        }
        let supported = if preset.supported_reasoning_efforts.is_empty() {
            std::iter::once(&preset.default_reasoning_effort).collect::<Vec<_>>()
        } else {
            preset
                .supported_reasoning_efforts
                .iter()
                .map(|option| &option.effort)
                .collect::<Vec<_>>()
        };
        if !supported.contains(&effort) {
            let supported = supported
                .iter()
                .map(|effort| effort.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "{} effort `{effort}` is unsupported by model `{model}` (supported: {supported})",
                role_label(role)
            ));
        }
        Ok(())
    }

    pub(super) fn dispatch_team_command(&mut self, args: &str) {
        let command = match parse_team_command(args) {
            Ok(command) => command,
            Err(message) => {
                self.add_error_message(message.to_string());
                return;
            }
        };
        let Some(thread_id) = self.thread_id() else {
            self.add_error_message(
                "Session is still starting; try /team again in a moment.".to_string(),
            );
            return;
        };
        self.app_event_tx
            .send(AppEvent::TeamCommand { thread_id, command });
    }

    pub(crate) fn show_team_status(&mut self) {
        self.add_info_message(format_team_status(self.team_settings.as_ref()), None);
    }

    pub(crate) fn open_team_balance_popup(&mut self) {
        let Some(thread_id) = self.thread_id() else {
            self.add_error_message(
                "Session is still starting; choose a Lead balance in a moment.".to_string(),
            );
            return;
        };
        let Some(team) = self.team_settings.as_ref() else {
            self.add_error_message("Team mode is not configured for this session.".to_string());
            return;
        };
        if team.role == Some(TeamRole::Worker) {
            self.add_error_message(
                "Lead usage/confidence balance can only be changed from a Lead session."
                    .to_string(),
            );
            return;
        }
        let current_balance = team
            .lead_balance
            .unwrap_or(codex_config::DEFAULT_TEAM_LEAD_BALANCE);
        let items = LEAD_BALANCE_OPTIONS
            .into_iter()
            .map(|(balance, label, description)| SelectionItem {
                name: label.to_string(),
                description: Some(description.to_string()),
                is_current: current_balance == balance,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::TeamCommand {
                        thread_id,
                        command: TeamCommand::ConfigureBalance { balance },
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            })
            .collect();
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Lead usage/confidence balance".to_string()),
            subtitle: Some(
                "Tune Lead oversight; Worker effort and required checks stay unchanged."
                    .to_string(),
            ),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn set_team_settings(&mut self, team_settings: Option<ThreadTeamSettings>) {
        self.team_settings = team_settings;
        self.request_redraw();
    }

    pub(crate) fn team_settings(&self) -> Option<&ThreadTeamSettings> {
        self.team_settings.as_ref()
    }

    pub(crate) fn team_command_is_already_applied(&self, command: &TeamCommand) -> bool {
        if self.pending_team_command.is_some() {
            return false;
        }
        let Some(team) = self.team_settings.as_ref() else {
            return false;
        };
        match command {
            TeamCommand::On => team.mode == TeamMode::LeadWorker,
            TeamCommand::Off => team.mode == TeamMode::Off,
            TeamCommand::Status
            | TeamCommand::SelectProfile { .. }
            | TeamCommand::SelectBalance => false,
            TeamCommand::ConfigureProfile {
                role,
                model,
                effort,
            } => team_profile_matches(team, *role, model, effort),
            TeamCommand::ConfigureBalance { balance } => {
                team.lead_balance
                    .unwrap_or(codex_config::DEFAULT_TEAM_LEAD_BALANCE)
                    == *balance
            }
        }
    }

    pub(crate) fn set_pending_team_command(&mut self, command: TeamCommand) {
        self.pending_team_command = Some(command);
    }

    pub(crate) fn clear_pending_team_command(&mut self) {
        self.pending_team_command = None;
    }

    /// Shows confirmation only when the server snapshot matches the requested mode.
    ///
    /// A settings notification can carry `team: None` while a thread is still being
    /// hydrated, so leave the request pending until a concrete team snapshot arrives.
    pub(crate) fn confirm_pending_team_command(&mut self) {
        let Some(command) = self.pending_team_command.clone() else {
            return;
        };
        let Some(team) = self.team_settings.as_ref() else {
            return;
        };
        let expected_mode = match command {
            TeamCommand::On => TeamMode::LeadWorker,
            TeamCommand::Off => TeamMode::Off,
            TeamCommand::Status
            | TeamCommand::SelectProfile { .. }
            | TeamCommand::SelectBalance => {
                self.pending_team_command = None;
                return;
            }
            TeamCommand::ConfigureProfile {
                role,
                model,
                effort,
            } => {
                if team_profile_matches(team, role, &model, &effort) {
                    self.add_info_message(format_team_status(Some(team)), None);
                    self.pending_team_command = None;
                }
                return;
            }
            TeamCommand::ConfigureBalance { balance } => {
                if team
                    .lead_balance
                    .unwrap_or(codex_config::DEFAULT_TEAM_LEAD_BALANCE)
                    == balance
                {
                    self.add_info_message(format_team_status(Some(team)), None);
                    self.pending_team_command = None;
                }
                return;
            }
        };
        if team.mode == expected_mode {
            self.add_info_message(format_team_status(Some(team)), None);
            self.pending_team_command = None;
        }
    }
}

fn team_profile_matches(
    team: &ThreadTeamSettings,
    role: TeamRole,
    model: &str,
    effort: &codex_protocol::openai_models::ReasoningEffort,
) -> bool {
    match role {
        TeamRole::Lead => {
            team.lead_model.as_deref() == Some(model)
                && team.lead_reasoning_effort.as_ref() == Some(effort)
        }
        TeamRole::Worker => {
            team.worker_model.as_deref() == Some(model)
                && team.worker_reasoning_effort.as_ref() == Some(effort)
        }
    }
}

pub(crate) fn format_team_status(team: Option<&ThreadTeamSettings>) -> String {
    let Some(team) = team else {
        return "Team mode is not configured for this session.".to_string();
    };

    let mode = match team.mode {
        TeamMode::Off => "off",
        TeamMode::LeadWorker => "on",
    };
    let mut lines = vec![format!("Lead/Worker team: {mode}")];
    if let Some(role) = team.role {
        lines.push(format!("Role: {}", role_label(role)));
    }
    if let Some(profile) = team_profile_line(
        "Lead",
        team.lead_model.as_deref(),
        team.lead_reasoning_effort.as_ref(),
    ) {
        lines.push(profile);
    }
    if let Some(profile) = team_profile_line(
        "Worker",
        team.worker_model.as_deref(),
        team.worker_reasoning_effort.as_ref(),
    ) {
        lines.push(profile);
    }
    if let Some(balance) = team
        .lead_balance
        .filter(|balance| *balance != codex_config::DEFAULT_TEAM_LEAD_BALANCE)
    {
        lines.push(format!(
            "Lead usage/confidence balance: {balance} ({})",
            lead_balance_label(balance)
        ));
    }
    lines.join("\n")
}

pub(crate) fn lead_balance_label(balance: u8) -> &'static str {
    LEAD_BALANCE_OPTIONS
        .iter()
        .find_map(|(option, label, _)| (*option == balance).then_some(*label))
        .unwrap_or("Unknown")
}

pub(crate) fn role_label(role: TeamRole) -> &'static str {
    match role {
        TeamRole::Lead => "Lead",
        TeamRole::Worker => "Worker",
    }
}

fn team_profile_line(
    label: &str,
    model: Option<&str>,
    effort: Option<&codex_protocol::openai_models::ReasoningEffort>,
) -> Option<String> {
    let model = model.filter(|model| !model.trim().is_empty())?;
    let effort = effort.map_or_else(
        || "default".to_string(),
        |effort| effort.as_str().to_string(),
    );
    Some(format!("{label}: {model} ({effort})"))
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
