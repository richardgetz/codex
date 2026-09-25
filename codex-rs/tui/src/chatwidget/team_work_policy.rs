//! Lead work-policy selection for an active Lead/Worker thread.

use super::ChatWidget;
use super::team::TeamCommand;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use codex_app_server_protocol::TeamLeadWorkPolicy;
use codex_app_server_protocol::TeamMode;
use codex_app_server_protocol::TeamRole;
use codex_app_server_protocol::ThreadTeamSettings;

pub(crate) fn is_active_lead_work_policy_target(
    team_settings: Option<&ThreadTeamSettings>,
) -> bool {
    matches!(
        team_settings,
        Some(settings)
            if settings.mode == TeamMode::LeadWorker && settings.role == Some(TeamRole::Lead)
    )
}

impl ChatWidget {
    pub(crate) fn open_team_work_policy_popup(&mut self) {
        let Some(thread_id) = self.thread_id() else {
            self.add_error_message(
                "Session is still starting; choose a Lead work policy in a moment.".to_string(),
            );
            return;
        };
        let Some(team) = self.team_settings.as_ref() else {
            self.add_error_message("Team mode is not configured for this session.".to_string());
            return;
        };
        if !is_active_lead_work_policy_target(Some(team)) {
            self.add_error_message(
                "Lead work policy can only be changed from an active Lead session.".to_string(),
            );
            return;
        }

        let current_policy = team
            .lead_work_policy
            .unwrap_or(TeamLeadWorkPolicy::PromptGuided);
        let items = [
            (
                TeamLeadWorkPolicy::PromptGuided,
                "Prompt guided",
                "Preserve the existing prompt-guided Lead behavior.",
            ),
            (
                TeamLeadWorkPolicy::ManagerOnly,
                "Manager only",
                "Keep the Lead focused on coordination, planning, and review; Workers execute assigned work.",
            ),
        ]
        .into_iter()
        .map(|(policy, name, description)| SelectionItem {
            name: name.to_string(),
            description: Some(description.to_string()),
            is_current: current_policy == policy,
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::TeamCommand {
                    thread_id,
                    command: TeamCommand::ConfigureWorkPolicy { policy },
                });
            })],
            dismiss_on_select: true,
            ..Default::default()
        })
        .collect();

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Lead work policy".to_string()),
            subtitle: Some(
                "Applies to this Lead thread, independent of its assigned model.".to_string(),
            ),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }
}

pub(crate) fn lead_work_policy_label(policy: TeamLeadWorkPolicy) -> &'static str {
    match policy {
        TeamLeadWorkPolicy::PromptGuided => "prompt guided",
        TeamLeadWorkPolicy::ManagerOnly => "manager only",
    }
}
