use super::EtaTab;
use super::EtaTaskStatus;
use super::EtaView;
use crate::app::eta_time::EtaTimestampFormatter;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::render::renderable::Renderable;
use crate::style::accent_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Margin;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

const ETA_AGENT_WIDTH: usize = 12;
const ETA_SESSION_WIDTH: usize = 24;
const ETA_STATUS_WIDTH: usize = 24;
const ETA_TIMING_WIDTH: usize = 22;
const ETA_COLUMN_GAP: usize = 2;

impl EtaView {
    fn footer(&self) -> Line<'static> {
        let mut spans = vec![
            "↑↓".bold(),
            " select  ".dim(),
            "←→/tab".bold(),
            " switch tab  ".dim(),
            "enter".bold(),
            " expand  ".dim(),
            "esc".bold(),
            " close".dim(),
        ];
        if self.tab == EtaTab::History && self.snapshot.next_cursor.is_some() {
            spans.extend(["  pgdn".bold(), " more history".dim()]);
        }
        if self.tab == EtaTab::AllSessions {
            spans.extend(["  r".bold(), " resume/retry".dim()]);
            spans.extend(["  n".bold(), " nested/flat".dim()]);
            if self.all_sessions_next_cursor.is_some() {
                spans.extend(["  pgdn".bold(), " more sessions".dim()]);
            }
        }
        spans.into()
    }

    fn render_tabs(&self) -> Line<'static> {
        let tab = |candidate: EtaTab| {
            if candidate == self.tab {
                format!("[{}]", candidate.label()).bold()
            } else {
                candidate.label().dim()
            }
        };
        vec![
            tab(EtaTab::Active),
            "  ".into(),
            tab(EtaTab::History),
            "  ".into(),
            tab(EtaTab::AllSessions),
        ]
        .into()
    }

    fn render_overall_lines(&self) -> Vec<Line<'static>> {
        let overall = &self.snapshot.overall;
        let remaining = format_range(
            overall.remaining_lower_seconds,
            overall.remaining_upper_seconds,
        );
        let finish = format_finish_range(
            self.snapshot.generated_at,
            overall,
            &self.timestamp_formatter,
        );
        let mut lines = vec![Line::from(vec![
            "Finish: ".dim(),
            finish.into(),
            "   Remaining: ".dim(),
            remaining.into(),
        ])];
        if let Some(reason) = &overall.unknown_reason {
            lines.push(Line::from(vec!["Overall: ".dim(), reason.clone().yellow()]));
        }
        lines
    }

    fn render_table_header(&self, width: usize) -> Line<'static> {
        if self.tab == EtaTab::AllSessions {
            let (title_width, session_width, status_width, timing_width) =
                session_column_widths(width);
            return Line::from(vec![
                format!("{:<title_width$}", "Task").bold(),
                " ".repeat(ETA_COLUMN_GAP).into(),
                format!("{:<width$}", "Session", width = session_width).bold(),
                " ".repeat(ETA_COLUMN_GAP).into(),
                format!("{:<width$}", "Status", width = status_width).bold(),
                " ".repeat(ETA_COLUMN_GAP).into(),
                format!("{:<width$}", "Remaining", width = timing_width).bold(),
            ]);
        }
        let title_width = table_title_width(width);
        let trailing = if self.tab == EtaTab::Active {
            "Remaining"
        } else {
            "Actual / original"
        };
        Line::from(vec![
            format!("{:<title_width$}", "Task").bold(),
            " ".repeat(ETA_COLUMN_GAP).into(),
            format!("{:<width$}", "Agent", width = ETA_AGENT_WIDTH).bold(),
            " ".repeat(ETA_COLUMN_GAP).into(),
            format!("{:<width$}", "Status", width = ETA_STATUS_WIDTH).bold(),
            " ".repeat(ETA_COLUMN_GAP).into(),
            format!("{trailing:<ETA_TIMING_WIDTH$}").bold(),
        ])
    }

    fn render_table_rows(&self, width: usize) -> Vec<Line<'static>> {
        if self.tab == EtaTab::AllSessions {
            return self.render_all_session_rows(width);
        }
        if self.tasks().is_empty() {
            if self.eta_request_in_flight {
                return vec!["Loading task estimates…".dim().into()];
            }
            if let Some(error) = self.eta_error.as_deref() {
                return vec![
                    format!("Unable to load task estimates: {error} · press r to retry")
                        .red()
                        .into(),
                ];
            }
            return vec!["No task estimates recorded for this session.".dim().into()];
        }
        let rows = self.ordered_indices();
        let title_width = table_title_width(width);
        let agent_width = ETA_AGENT_WIDTH;
        let mut lines = Vec::with_capacity(rows.len() + 1);
        if self.eta_request_in_flight {
            lines.push("Refreshing task estimates…".dim().into());
        }
        if let Some(error) = self.eta_error.as_deref() {
            lines.push(
                format!("Last refresh failed: {error} · press r to retry")
                    .red()
                    .into(),
            );
        }
        for (display_idx, task_idx) in rows.into_iter().enumerate() {
            let task = &self.tasks()[task_idx];
            let depth = self.task_depth(task_idx);
            let has_children = self.tasks().iter().any(|candidate| {
                candidate.parent_task_id.as_deref() == Some(task.task_id.as_str())
            });
            let marker = if has_children {
                if self.collapsed_task_ids.contains(&task.task_id) {
                    "▸ "
                } else {
                    "▾ "
                }
            } else {
                "  "
            };
            let prefix = format!("{}{}", "  ".repeat(depth), marker);
            let title = format!("{prefix}{}", task.title.trim());
            let title = fit_text(&title, title_width);
            let agent = fit_text(
                &agent_label(&task.owner_thread_id, &self.snapshot.root_thread_id),
                agent_width,
            );
            let mut status = task.status.label().to_string();
            if self.tab == EtaTab::History && task.status != EtaTaskStatus::Cancelled {
                status.push_str(" · ");
                status.push_str(task.accuracy.label());
            }
            if task.is_stale {
                status.push_str(" · ⚠");
            }
            let status = fit_text(&status, ETA_STATUS_WIDTH);
            let estimate = if self.tab == EtaTab::Active {
                active_remaining_label(task)
            } else {
                let actual = task
                    .actual_elapsed_seconds
                    .map(format_seconds)
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "{actual} / {}",
                    format_range(task.original_lower_seconds, task.original_upper_seconds)
                )
            };
            let estimate = fit_text(&estimate, ETA_TIMING_WIDTH);
            let line = Line::from(vec![
                Span::from(format!("{title:<title_width$}")),
                " ".repeat(ETA_COLUMN_GAP).into(),
                Span::from(format!("{agent:<agent_width$}")),
                " ".repeat(ETA_COLUMN_GAP).into(),
                Span::from(format!("{status:<ETA_STATUS_WIDTH$}")),
                " ".repeat(ETA_COLUMN_GAP).into(),
                Span::from(format!("{estimate:<ETA_TIMING_WIDTH$}")),
            ]);
            let line = truncate_line_with_ellipsis_if_overflow(line, width);
            lines.push(if self.state.selected_idx == Some(display_idx) {
                line.patch_style(accent_style())
            } else if task.is_stale {
                line.yellow()
            } else {
                line
            });
        }
        lines
    }

    fn render_details(&self, width: usize) -> Vec<Line<'static>> {
        if self.tab == EtaTab::AllSessions {
            return self.render_all_session_details(width);
        }
        let Some(task) = self.selected_task() else {
            return vec!["No task selected.".dim().into()];
        };
        let mut lines = vec![Line::from("Task details".bold())];
        lines.push(detail_line("Title", task.title.trim()));
        lines.push(detail_line(
            "Agent",
            &agent_label(&task.owner_thread_id, &self.snapshot.root_thread_id),
        ));
        let status = if task.status == EtaTaskStatus::Cancelled {
            task.status.label().to_string()
        } else if self.tab == EtaTab::History {
            format!("{} · {}", task.status.label(), task.accuracy.label())
        } else {
            task.status.label().to_string()
        };
        lines.push(detail_line("Status", &status));
        let started = task
            .started_at
            .map(|timestamp| self.timestamp_formatter.format(timestamp))
            .unwrap_or_else(|| "not started".to_string());
        let ended = match task.terminal_at {
            Some(timestamp) => self.timestamp_formatter.format(timestamp),
            None if task.started_at.is_some() => "in progress".to_string(),
            None => "not started".to_string(),
        };
        lines.push(detail_line("Started", &started));
        lines.push(detail_line("Ended", &ended));
        let current = if self.tab == EtaTab::Active {
            active_remaining_label(task)
        } else {
            format_range(task.current_lower_seconds, task.current_upper_seconds)
        };
        lines.push(detail_line("Current", &current));
        lines.push(detail_line(
            "Original",
            &format_range(task.original_lower_seconds, task.original_upper_seconds),
        ));
        lines.push(detail_line(
            "Actual",
            &task
                .actual_elapsed_seconds
                .map(format_seconds)
                .unwrap_or_else(|| "unknown".to_string()),
        ));
        lines.push(detail_line(
            "Updated",
            &self.timestamp_formatter.format(task.updated_at),
        ));
        if task.is_stale {
            lines.push(
                "⚠ May be outdated; the saved estimate remains visible until its owner updates it."
                    .yellow()
                    .into(),
            );
        }
        if !task.depends_on_task_ids.is_empty() {
            let dependencies = task
                .depends_on_task_ids
                .iter()
                .map(|id| short_id(id))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(detail_line("Depends on", &dependencies));
        }
        if let Some(parent) = task.parent_task_id.as_deref() {
            lines.push(detail_line("Parent", &short_id(parent)));
        }
        if !task.revisions.is_empty() {
            lines.push(detail_line("Revisions", &task.revisions.len().to_string()));
            for (index, revision) in task.revisions.iter().enumerate() {
                let estimate = format_range(revision.lower_seconds, revision.upper_seconds);
                let actor = short_id(&revision.actor_thread_id);
                let value = revision
                    .reason
                    .as_deref()
                    .map(|reason| {
                        format!(
                            "{estimate} · {} · by {actor} · {reason}",
                            self.timestamp_formatter.format(revision.updated_at)
                        )
                    })
                    .unwrap_or_else(|| {
                        format!(
                            "{estimate} · {} · by {actor}",
                            self.timestamp_formatter.format(revision.updated_at)
                        )
                    });
                lines.push(detail_line(&format!("  #{}", index + 1), &value));
            }
        }
        lines
            .into_iter()
            .map(|line| truncate_line_with_ellipsis_if_overflow(line, width))
            .collect()
    }

    fn render_all_session_rows(&self, width: usize) -> Vec<Line<'static>> {
        if self.all_sessions.is_empty() {
            if self.all_sessions_request_in_flight {
                return vec!["Loading retained sessions…".dim().into()];
            }
            if let Some(error) = self.all_sessions_error.as_deref() {
                return vec![
                    format!("Unable to load retained sessions: {error} · press r to retry")
                        .red()
                        .into(),
                ];
            }
            return vec!["No retained ETA sessions found.".dim().into()];
        }
        let rows = self.ordered_session_indices();
        let (title_width, session_width, status_width, timing_width) = session_column_widths(width);
        let mut lines = Vec::with_capacity(rows.len() + 1);
        if self.all_sessions_request_in_flight {
            lines.push("Refreshing retained sessions…".dim().into());
        }
        if let Some(error) = self.all_sessions_error.as_deref() {
            lines.push(
                format!("Last refresh failed: {error} · press r to retry")
                    .red()
                    .into(),
            );
        }
        let mut previous_root: Option<&str> = None;
        for (display_idx, task_idx) in rows.into_iter().enumerate() {
            let task = &self.session_tasks()[task_idx];
            if previous_root != Some(task.root_thread_id.as_str()) {
                let session_label = session_label(task);
                let heading = format!(
                    "Session: {session_label}  {}",
                    short_id(&task.root_thread_id)
                );
                lines.push(truncate_line_with_ellipsis_if_overflow(
                    Line::from(heading.bold()),
                    width,
                ));
                previous_root = Some(task.root_thread_id.as_str());
            }
            let depth = self.session_task_depth(task_idx);
            let has_children = task.nested_task_count > 0;
            let marker = if has_children {
                if !self.all_sessions_include_nested
                    || self
                        .collapsed_session_task_ids
                        .contains(&(task.root_thread_id.clone(), task.task_id.clone()))
                {
                    "▸ "
                } else {
                    "▾ "
                }
            } else {
                "  "
            };
            let indent = "  ".repeat(depth);
            let prefix = format!("{indent}{marker}");
            let nested_badge = (task.nested_task_count > 0)
                .then(|| {
                    format!(
                        "  [{} nested{}]",
                        task.nested_task_count,
                        (task.active_nested_task_count > 0)
                            .then(|| format!(", {} active", task.active_nested_task_count))
                            .unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            let title = fit_text(
                &format!("{prefix}{}{nested_badge}", task.title.trim()),
                title_width,
            );
            let current = self
                .current_thread_id
                .is_some_and(|thread_id| thread_id.to_string() == task.root_thread_id);
            let mut session = session_label(task);
            if current {
                session.push_str(" · current");
            }
            let session = fit_text(&session, session_width);
            let mut status = task.status.label().to_string();
            if task.is_stale {
                if status.width() + 4 > status_width {
                    status = format!("⚠ {status}");
                } else {
                    status.push_str(" · ⚠");
                }
            }
            let status = fit_text(&status, status_width);
            let remaining = if task.nested_task_count > 0 {
                let own = format_range(task.current_lower_seconds, task.current_upper_seconds);
                let nested = format_range(task.nested_lower_seconds, task.nested_upper_seconds);
                format!("{own} (+{nested})")
            } else {
                format_range(task.current_lower_seconds, task.current_upper_seconds)
            };
            let remaining = fit_text(&remaining, timing_width);
            let line = Line::from(vec![
                Span::from(format!("{title:<title_width$}")),
                " ".repeat(ETA_COLUMN_GAP).into(),
                Span::from(format!("{session:<session_width$}")),
                " ".repeat(ETA_COLUMN_GAP).into(),
                Span::from(format!("{status:<status_width$}")),
                " ".repeat(ETA_COLUMN_GAP).into(),
                Span::from(format!("{remaining:<timing_width$}")),
            ]);
            let line = truncate_line_with_ellipsis_if_overflow(line, width);
            lines.push(if self.state.selected_idx == Some(display_idx) {
                line.patch_style(accent_style())
            } else if task.is_stale {
                line.yellow()
            } else {
                line
            });
        }
        lines
    }

    fn render_all_session_details(&self, width: usize) -> Vec<Line<'static>> {
        let Some(task) = self.selected_session_task() else {
            return vec!["No session task selected.".dim().into()];
        };
        let session = &task.session;
        let status = task.status.label();
        let mut lines = vec![Line::from("Session task details".bold())];
        lines.push(detail_line("Task", task.title.trim()));
        lines.push(detail_line("Status", status));
        if task.is_stale {
            lines.push(
                "⚠ May be outdated; the saved estimate remains visible until its owner updates it."
                    .yellow()
                    .into(),
            );
        }
        lines.push(detail_line("Session", &session_label(task)));
        if let Some(preview) = task
            .session
            .preview
            .as_deref()
            .filter(|preview| !preview.trim().is_empty())
        {
            lines.push(detail_line("Preview", preview.trim()));
        }
        lines.push(detail_line("Session ID", &short_id(&session.thread_id)));
        lines.push(detail_line("Working directory", &session.cwd));
        if session.created_at > 0 {
            lines.push(detail_line(
                "Created",
                &self.timestamp_formatter.format(session.created_at),
            ));
        }
        if session.updated_at > 0 {
            lines.push(detail_line(
                "Updated",
                &self.timestamp_formatter.format(session.updated_at),
            ));
        }
        if let Some(archived_at) = session.archived_at {
            lines.push(detail_line(
                "Archived",
                &self.timestamp_formatter.format(archived_at),
            ));
        }
        lines.push(detail_line(
            "Nested",
            &format!(
                "{} total · {} active",
                task.nested_task_count, task.active_nested_task_count
            ),
        ));
        lines
            .into_iter()
            .map(|line| truncate_line_with_ellipsis_if_overflow(line, width))
            .collect()
    }

    fn all_session_scroll_offset(&self) -> usize {
        let rows = self.ordered_session_indices();
        let status_lines = if self.all_sessions.is_empty() {
            0
        } else {
            usize::from(self.all_sessions_request_in_flight)
                + usize::from(self.all_sessions_error.is_some())
        };
        let mut offset = if self.state.scroll_top > 0 {
            status_lines
        } else {
            0
        };
        let mut previous_root: Option<&str> = None;
        for (display_idx, task_idx) in rows.into_iter().enumerate() {
            if display_idx >= self.state.scroll_top {
                break;
            }
            let task = &self.session_tasks()[task_idx];
            if previous_root != Some(task.root_thread_id.as_str()) {
                offset += 1;
                previous_root = Some(task.root_thread_id.as_str());
            }
            offset += 1;
        }
        offset
    }
}

impl Renderable for EtaView {
    fn desired_height(&self, _width: u16) -> u16 {
        24
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 24 || area.height < 8 {
            return;
        }
        Clear.render(area, buf);
        let [content_area, footer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
        let content_area = crate::bottom_pane::render_menu_surface(content_area, buf);
        let inset =
            |rect: Rect| rect.inner(Margin::new(/*horizontal*/ 1, /*vertical*/ 0));
        let [
            header_area,
            summary_area,
            tabs_area,
            divider_area,
            body_area,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(content_area);
        Line::from("Task estimates".bold()).render(inset(header_area), buf);
        let mut summary = if self.tab == EtaTab::AllSessions {
            vec![Line::from(
                format!(
                    "{} tasks   {} nested loaded   mode: {}",
                    self.all_sessions.len(),
                    self.all_sessions
                        .iter()
                        .filter(|task| task.parent_task_id.is_some())
                        .count(),
                    if self.all_sessions_include_nested {
                        "nested"
                    } else {
                        "top-level"
                    },
                )
                .dim(),
            )]
        } else {
            vec![Line::from(
                format!(
                    "{} active   {} history   updated {}",
                    self.snapshot.active.len(),
                    self.snapshot.history.len(),
                    if self.snapshot.generated_at > 0 {
                        self.timestamp_formatter.format(self.snapshot.generated_at)
                    } else {
                        "unknown".to_string()
                    },
                )
                .dim(),
            )]
        };
        if self.tab != EtaTab::AllSessions {
            summary.extend(self.render_overall_lines());
        }
        Paragraph::new(summary).render(inset(summary_area), buf);
        self.render_tabs().render(inset(tabs_area), buf);
        Line::from(
            "─"
                .repeat(usize::from(content_area.width.saturating_sub(2)))
                .dim(),
        )
        .render(inset(divider_area), buf);

        let body_area = inset(body_area);
        let width = body_area.width as usize;
        let expanded_table_height =
            (1 + self.visible_rows() as u16).min(body_area.height.saturating_sub(1).max(1));
        let [table_area, detail_area] = if self.expanded {
            Layout::vertical([
                Constraint::Length(expanded_table_height),
                Constraint::Fill(1),
            ])
            .areas(body_area)
        } else {
            Layout::vertical([Constraint::Fill(1), Constraint::Length(0)]).areas(body_area)
        };
        let mut table_lines = vec![self.render_table_header(width)];
        let rows = self.render_table_rows(width);
        let scroll_top = if self.tab == EtaTab::AllSessions {
            self.all_session_scroll_offset()
        } else {
            self.state.scroll_top
        };
        table_lines.extend(
            rows.into_iter()
                .skip(scroll_top)
                .take(table_area.height.saturating_sub(1) as usize),
        );
        Paragraph::new(table_lines).render(table_area, buf);
        if self.expanded && !detail_area.is_empty() {
            let mut details = vec![Line::default()];
            details.extend(self.render_details(detail_area.width as usize));
            Paragraph::new(details).render(detail_area, buf);
        }
        self.footer().dim().render(footer_area, buf);
    }
}

fn detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![format!("{label}: ").dim(), value.to_string().into()])
}

fn table_title_width(width: usize) -> usize {
    width
        .saturating_sub(
            ETA_AGENT_WIDTH + ETA_STATUS_WIDTH + ETA_TIMING_WIDTH + (ETA_COLUMN_GAP * 3),
        )
        .max(8)
}

fn session_table_title_width(width: usize) -> usize {
    width
        .saturating_sub(
            ETA_SESSION_WIDTH + ETA_STATUS_WIDTH + ETA_TIMING_WIDTH + (ETA_COLUMN_GAP * 3),
        )
        .max(8)
}

fn session_column_widths(width: usize) -> (usize, usize, usize, usize) {
    if width < 56 {
        return (8, 8, 8, 10);
    }
    if width < 96 {
        let session = 16;
        let status = 16;
        let timing = 18;
        let title = width
            .saturating_sub(session + status + timing + (ETA_COLUMN_GAP * 3))
            .max(8);
        return (title, session, status, timing);
    }
    (
        session_table_title_width(width),
        ETA_SESSION_WIDTH,
        ETA_STATUS_WIDTH,
        ETA_TIMING_WIDTH,
    )
}

fn session_label(task: &super::EtaSessionTask) -> String {
    if let Some(name) = task
        .session
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    {
        return name.trim().to_string();
    }
    if !task.session.title.trim().is_empty() {
        return task.session.title.trim().to_string();
    }
    if let Some(preview) = task
        .session
        .preview
        .as_deref()
        .filter(|preview| !preview.trim().is_empty())
    {
        return preview.trim().to_string();
    }
    short_id(&task.root_thread_id)
}

fn active_remaining_label(task: &super::EtaTask) -> String {
    // The app-server projection already evaluates active estimates at its
    // snapshot timestamp. Keep that from-now range intact so revisions do not
    // get subtracted again from their original start time. A stale row carries
    // the saved range by design; the warning marker and details explain that it
    // needs owner reassessment.
    format_range(task.current_lower_seconds, task.current_upper_seconds)
}

fn fit_text(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut output = String::new();
    let mut used = 0;
    for ch in value.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width + 1 > width {
            break;
        }
        output.push(ch);
        used += ch_width;
    }
    output.push('…');
    output
}

fn short_id(value: &str) -> String {
    value.chars().take(8).collect()
}

fn agent_label(owner_thread_id: &str, root_thread_id: &str) -> String {
    if owner_thread_id == root_thread_id {
        "Lead".to_string()
    } else {
        format!("Worker {}", short_id(owner_thread_id))
    }
}

fn format_range(lower: Option<i64>, upper: Option<i64>) -> String {
    match (lower, upper) {
        (Some(lower), Some(upper)) if lower == upper => format_seconds(lower),
        (Some(lower), Some(upper)) => {
            format!("{}–{}", format_seconds(lower), format_seconds(upper))
        }
        (Some(lower), None) => format!("≥{}", format_seconds(lower)),
        (None, Some(upper)) => format!("≤{}", format_seconds(upper)),
        (None, None) => "unknown".to_string(),
    }
}

fn format_seconds(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h {}m", minutes % 60);
    }
    format!("{hours}h {}m", minutes % 60)
}

fn format_finish_range(
    generated_at: i64,
    overall: &super::EtaOverall,
    timestamp_formatter: &EtaTimestampFormatter,
) -> String {
    if overall.unknown_reason.is_some() {
        return "unknown".to_string();
    }

    match (
        overall.remaining_lower_seconds,
        overall.remaining_upper_seconds,
    ) {
        (Some(lower), Some(upper)) if generated_at > 0 && lower >= 0 && upper >= lower => {
            let Some(lower_finish) = generated_at.checked_add(lower) else {
                return "unknown".to_string();
            };
            let Some(upper_finish) = generated_at.checked_add(upper) else {
                return "unknown".to_string();
            };
            if lower == 0 && upper == 0 {
                format!("≤{}", timestamp_formatter.format(upper_finish))
            } else if lower_finish == upper_finish {
                timestamp_formatter.format(lower_finish)
            } else {
                format!(
                    "{}–{}",
                    timestamp_formatter.format(lower_finish),
                    timestamp_formatter.format(upper_finish)
                )
            }
        }
        (Some(lower), None) if generated_at > 0 && lower >= 0 => generated_at
            .checked_add(lower)
            .map(|finish| format!("≥{}", timestamp_formatter.format(finish)))
            .unwrap_or_else(|| "unknown".to_string()),
        (None, Some(upper)) if generated_at > 0 && upper >= 0 => generated_at
            .checked_add(upper)
            .map(|finish| format!("≤{}", timestamp_formatter.format(finish)))
            .unwrap_or_else(|| "unknown".to_string()),
        _ => overall
            .finish_at
            .filter(|finish| *finish > 0)
            .map(|finish| format!("≤{}", timestamp_formatter.format(finish)))
            .unwrap_or_else(|| "unknown".to_string()),
    }
}
