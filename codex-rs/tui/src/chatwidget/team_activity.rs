//! Session-wide Lead/Worker activity presentation.
//!
//! The app layer owns the event-driven projection of every thread in the
//! current team. `ChatWidget` only keeps the compact presentation state and
//! forwards it to the bottom pane, so the local Lead turn flag remains
//! independent from Worker activity.

/// Activity reported for the Lead or one Worker thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TeamRoleActivity {
    #[default]
    Idle,
    Working,
    Waiting,
}

/// Root-scoped pause state reported by the app server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TeamPauseState {
    #[default]
    Running,
    Pausing,
    Paused,
}

/// Compact activity projection for one Lead/Worker session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TeamActivityStatus {
    pub(crate) lead: TeamRoleActivity,
    pub(crate) workers_working: usize,
    pub(crate) workers_waiting: usize,
    pub(crate) pause_state: TeamPauseState,
    pub(crate) in_flight_operations: u32,
}

impl TeamActivityStatus {
    /// Return whether this projection should occupy a status row.
    pub(crate) fn is_visible(self) -> bool {
        self.pause_state != TeamPauseState::Running
            || self.lead != TeamRoleActivity::Idle
            || self.workers_working > 0
            || self.workers_waiting > 0
    }

    /// Return whether the status row should animate.
    pub(crate) fn is_animated(self) -> bool {
        match self.pause_state {
            TeamPauseState::Running => {
                self.lead == TeamRoleActivity::Working || self.workers_working > 0
            }
            TeamPauseState::Pausing => self.in_flight_operations > 0,
            TeamPauseState::Paused => false,
        }
    }

    /// Render the compact status row shown above the composer.
    pub(crate) fn header(self) -> String {
        match self.pause_state {
            TeamPauseState::Pausing => {
                if self.in_flight_operations == 0 {
                    String::from("Pausing — finishing in-flight work")
                } else {
                    format!(
                        "Pausing — finishing {} in-flight operation{}",
                        self.in_flight_operations,
                        if self.in_flight_operations == 1 {
                            ""
                        } else {
                            "s"
                        }
                    )
                }
            }
            TeamPauseState::Paused => {
                let workers = self.workers_working.saturating_add(self.workers_waiting);
                if workers == 0 {
                    String::from("Paused — /continue to resume")
                } else {
                    format!(
                        "Paused · Lead + {workers} worker{} · /continue to resume",
                        if workers == 1 { "" } else { "s" }
                    )
                }
            }
            TeamPauseState::Running => {
                let lead = match self.lead {
                    TeamRoleActivity::Idle => "idle",
                    TeamRoleActivity::Working => "working",
                    TeamRoleActivity::Waiting => "waiting",
                };
                let mut workers = format!("{} working", self.workers_working);
                if self.workers_waiting > 0 {
                    workers.push_str(&format!(", {} waiting", self.workers_waiting));
                }
                format!("Lead: {lead} · Workers: {workers}")
            }
        }
    }
}

impl super::ChatWidget {
    /// Apply the current root-scoped activity projection to the status row.
    pub(crate) fn set_team_activity(&mut self, status: Option<TeamActivityStatus>) {
        self.team_activity_status = status.filter(|status| status.is_visible());
        let Some(status) = self.team_activity_status else {
            self.bottom_pane.set_team_activity(None, false);
            self.refresh_status_surfaces();
            return;
        };
        self.bottom_pane
            .set_team_activity(Some(status.header()), status.is_animated());
        self.refresh_status_surfaces();
    }

    #[cfg(test)]
    pub(crate) fn team_activity_status_header(&self) -> Option<String> {
        self.bottom_pane
            .status_widget()
            .map(|status| status.header().to_string())
    }
}
