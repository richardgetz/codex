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
    /// Aggregate activity for all unfinished direct and nested Workers.
    pub(crate) workers_working: usize,
    pub(crate) workers_waiting: usize,
    /// Number of unfinished direct children of the Lead.
    pub(crate) direct_workers: usize,
    /// Number of unfinished descendants below direct Workers.
    pub(crate) subagents: usize,
    /// Optional configured direct Worker concurrency ceiling.
    pub(crate) worker_max_concurrent: Option<usize>,
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

    /// Render the first status row shown above the composer.
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
                format!("Lead: {lead} · Team: {}", self.team_activity_label())
            }
        }
    }

    /// Render the second status row for a running team, if one is needed.
    pub(crate) fn details(self) -> Option<String> {
        (self.pause_state == TeamPauseState::Running).then(|| {
            let workers = self.worker_max_concurrent.map_or_else(
                || self.direct_workers.to_string(),
                |max| format!("{}/{}", self.direct_workers, max),
            );
            format!("  Workers: {workers} · Subagents: {}", self.subagents)
        })
    }

    /// Render the one-line status used by configurable terminal-title/status surfaces.
    ///
    /// The composer gets a two-row layout so the two columns can adapt to the terminal width;
    /// title/status-line items remain single-line strings by design.
    pub(crate) fn title(self) -> String {
        match self.details() {
            Some(details) => format!("{} · {}", self.header(), details.trim_start()),
            None => self.header(),
        }
    }

    /// Render the two aligned activity columns for the current width.
    ///
    /// The status indicator adds its spinner to the first row. Keeping this helper independent of
    /// ratatui lets the projection remain display-only and makes narrow-width behavior testable
    /// without a terminal.
    pub(crate) fn lines(self, width: u16) -> Vec<String> {
        if self.pause_state != TeamPauseState::Running {
            return vec![self.header()];
        }

        let lead = match self.lead {
            TeamRoleActivity::Idle => "idle",
            TeamRoleActivity::Working => "working",
            TeamRoleActivity::Waiting => "waiting",
        };
        let team = format!("Team: {}", self.team_activity_label());
        let workers = self.worker_max_concurrent.map_or_else(
            || self.direct_workers.to_string(),
            |max| format!("{}/{}", self.direct_workers, max),
        );
        let subagents = format!("Subagents: {}", self.subagents);
        let left_top = format!("Lead: {lead}");
        let left_bottom = format!("Workers: {workers}");
        let intrinsic_left_width = left_top.len().max(left_bottom.len());
        let full_width = intrinsic_left_width + 3 + team.len();

        // Preserve aligned columns whenever the complete first row fits. For narrow terminals,
        // avoid spending the whole row on padding; the enclosing status renderer truncates the
        // right-hand labels with its normal ellipsis behavior.
        let left_width = if usize::from(width) >= full_width {
            intrinsic_left_width
        } else {
            left_top
                .len()
                .max(left_bottom.len())
                .min(usize::from(width))
        };
        vec![
            format!("{left_top:<left_width$} · {team}"),
            format!("{left_bottom:<left_width$} · {subagents}"),
        ]
    }

    fn team_activity_label(self) -> String {
        let mut label = format!("{} working", self.workers_working);
        if self.workers_waiting > 0 {
            label.push_str(&format!(", {} waiting", self.workers_waiting));
        }
        label
    }
}

impl super::ChatWidget {
    /// Apply the current root-scoped activity projection to the status row.
    pub(crate) fn set_team_activity(&mut self, status: Option<TeamActivityStatus>) {
        let status = status.filter(|status| status.is_visible());
        if self.team_activity_status == status {
            return;
        }
        self.team_activity_status = status;
        let Some(status) = self.team_activity_status else {
            self.bottom_pane.set_team_activity(None);
            self.refresh_status_surfaces();
            return;
        };
        self.bottom_pane.set_team_activity(Some(status));
        self.refresh_status_surfaces();
    }

    #[cfg(test)]
    pub(crate) fn team_activity_status_header(&self) -> Option<String> {
        self.bottom_pane
            .status_widget()
            .map(|status| status.header().to_string())
    }
}
