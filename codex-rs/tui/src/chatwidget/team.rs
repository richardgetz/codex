//! Slash-command parsing and authoritative status rendering for Lead/Worker teams.

use super::ChatWidget;
use crate::app_event::AppEvent;
use codex_app_server_protocol::TeamMode;
use codex_app_server_protocol::TeamRole;
use codex_app_server_protocol::ThreadTeamSettings;

pub(crate) const TEAM_USAGE: &str = "Usage: /team [on|off|status]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TeamCommand {
    On,
    Off,
    Status,
}

pub(crate) fn parse_team_command(args: &str) -> Result<TeamCommand, &'static str> {
    match args.trim().to_ascii_lowercase().as_str() {
        "" | "status" => Ok(TeamCommand::Status),
        "on" => Ok(TeamCommand::On),
        "off" => Ok(TeamCommand::Off),
        _ => Err(TEAM_USAGE),
    }
}

impl ChatWidget {
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

    pub(crate) fn set_team_settings(&mut self, team_settings: Option<ThreadTeamSettings>) {
        self.team_settings = team_settings;
        self.request_redraw();
    }

    pub(crate) fn team_settings(&self) -> Option<&ThreadTeamSettings> {
        self.team_settings.as_ref()
    }

    pub(crate) fn team_command_is_already_applied(&self, command: TeamCommand) -> bool {
        if self.pending_team_command.is_some() {
            return false;
        }
        let Some(team) = self.team_settings.as_ref() else {
            return false;
        };
        match command {
            TeamCommand::On => team.mode == TeamMode::LeadWorker,
            TeamCommand::Off => team.mode == TeamMode::Off,
            TeamCommand::Status => false,
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
        let Some(command) = self.pending_team_command else {
            return;
        };
        let Some(team) = self.team_settings.as_ref() else {
            return;
        };
        let expected_mode = match command {
            TeamCommand::On => TeamMode::LeadWorker,
            TeamCommand::Off => TeamMode::Off,
            TeamCommand::Status => {
                self.pending_team_command = None;
                return;
            }
        };
        if team.mode == expected_mode {
            self.add_info_message(format_team_status(Some(team)), None);
            self.pending_team_command = None;
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
    lines.join("\n")
}

fn role_label(role: TeamRole) -> &'static str {
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
