use super::EtaTaskStatus;
use super::EtaTab;
use super::EtaView;
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
const ETA_STATUS_WIDTH: usize = 32;

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
        vec![tab(EtaTab::Active), "  ".into(), tab(EtaTab::History)].into()
    }

    fn render_overall_lines(&self) -> Vec<Line<'static>> {
        let overall = &self.snapshot.overall;
        let remaining = format_range(
            overall.remaining_lower_seconds,
            overall.remaining_upper_seconds,
        );
        let finish = format_finish_range(self.snapshot.generated_at, overall);
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

    fn render_table_header(&self, title_width: usize, agent_width: usize) -> Line<'static> {
        let trailing = if self.tab == EtaTab::Active {
            "Remaining"
        } else {
            "Actual / original"
        };
        Line::from(vec![
            format!("{:<title_width$}", "Task").bold(),
            "  ".into(),
            format!("{:<agent_width$}", "Agent").bold(),
            "  ".into(),
            format!("{:<width$}", "Status", width = ETA_STATUS_WIDTH).bold(),
            "  ".into(),
            trailing.bold(),
        ])
    }

    fn render_table_rows(&self, width: usize) -> Vec<Line<'static>> {
        let rows = self.ordered_indices();
        let title_width = width
            .saturating_sub(/*gaps*/ 2 + ETA_AGENT_WIDTH + ETA_STATUS_WIDTH + 2)
            .max(8);
        let agent_width = ETA_AGENT_WIDTH;
        let mut lines = Vec::with_capacity(rows.len());
        for (display_idx, task_idx) in rows.into_iter().enumerate() {
            let task = &self.tasks()[task_idx];
            let depth = self.task_depth(task_idx);
            let has_children = self
                .tasks()
                .iter()
                .any(|candidate| candidate.parent_task_id.as_deref() == Some(task.task_id.as_str()));
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
            let agent = fit_text(&agent_label(&task.owner_thread_id, &self.snapshot.root_thread_id), agent_width);
            let mut status = task.status.label().to_string();
            if self.tab == EtaTab::History && task.status != EtaTaskStatus::Cancelled {
                status.push_str(" · ");
                status.push_str(task.accuracy.label());
            }
            if task.is_stale {
                status.push_str(" · stale");
            }
            let status = fit_text(&status, ETA_STATUS_WIDTH);
            let estimate = if self.tab == EtaTab::Active {
                format_range(task.current_lower_seconds, task.current_upper_seconds)
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
            let line = Line::from(vec![
                Span::from(format!("{title:<title_width$}")),
                "  ".into(),
                Span::from(format!("{agent:<agent_width$}")),
                "  ".into(),
                Span::from(format!("{status:<width$}", width = ETA_STATUS_WIDTH)),
                "  ".into(),
                Span::from(estimate),
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
        lines.push(detail_line(
            "Current",
            &format_range(task.current_lower_seconds, task.current_upper_seconds),
        ));
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
        lines.push(detail_line("Updated", &format_timestamp(task.updated_at)));
        if task.is_stale {
            lines.push("Snapshot is stale; waiting for a stored update.".yellow().into());
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
                            format_timestamp(revision.updated_at)
                        )
                    })
                    .unwrap_or_else(|| {
                        format!(
                            "{estimate} · {} · by {actor}",
                            format_timestamp(revision.updated_at)
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
        let content_area = crate::bottom_pane::selection_popup_common::render_menu_surface(
            content_area,
            buf,
        );
        let inset = |rect: Rect| rect.inner(Margin::new(/*horizontal*/ 1, /*vertical*/ 0));
        let [header_area, summary_area, tabs_area, divider_area, body_area] =
            Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Fill(1),
            ])
            .areas(content_area);
        Line::from("Task estimates".bold()).render(inset(header_area), buf);
        let mut summary = vec![Line::from(vec![
            format!(
                "{} active   {} history   seq {}   updated {}",
                self.snapshot.active.len(),
                self.snapshot.history.len(),
                self.snapshot.sequence,
                if self.snapshot.generated_at > 0 {
                    format_timestamp(self.snapshot.generated_at)
                } else {
                    "unknown".to_string()
                },
            )
            .dim(),
        ])];
        summary.extend(self.render_overall_lines());
        Paragraph::new(summary).render(inset(summary_area), buf);
        self.render_tabs().render(inset(tabs_area), buf);
        Line::from("─".repeat(usize::from(content_area.width.saturating_sub(2))).dim())
            .render(inset(divider_area), buf);

        let body_area = inset(body_area);
        let width = body_area.width as usize;
        let title_width = width
            .saturating_sub(/*gaps*/ 2 + ETA_AGENT_WIDTH + ETA_STATUS_WIDTH + 2)
            .max(8);
        let expanded_table_height = (1 + self.visible_rows() as u16)
            .min(body_area.height.saturating_sub(1).max(1));
        let [table_area, detail_area] = if self.expanded {
            Layout::vertical([Constraint::Length(expanded_table_height), Constraint::Fill(1)])
                .areas(body_area)
        } else {
            Layout::vertical([Constraint::Fill(1), Constraint::Length(0)]).areas(body_area)
        };
        let mut table_lines = vec![self.render_table_header(title_width, ETA_AGENT_WIDTH)];
        let rows = self.render_table_rows(width);
        table_lines.extend(
            rows.into_iter()
                .skip(self.state.scroll_top)
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
        (Some(lower), Some(upper)) => format!("{}–{}", format_seconds(lower), format_seconds(upper)),
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

fn format_timestamp(seconds: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("unix {seconds}"))
}

fn format_finish_range(generated_at: i64, overall: &super::EtaOverall) -> String {
    if overall.unknown_reason.is_some() {
        return "unknown".to_string();
    }

    match (
        overall.remaining_lower_seconds,
        overall.remaining_upper_seconds,
    ) {
        (Some(lower), Some(upper))
            if generated_at > 0 && lower >= 0 && upper >= lower =>
        {
            let Some(lower_finish) = generated_at.checked_add(lower) else {
                return "unknown".to_string();
            };
            let Some(upper_finish) = generated_at.checked_add(upper) else {
                return "unknown".to_string();
            };
            if lower == 0 && upper == 0 {
                format!("≤{}", format_timestamp(upper_finish))
            } else if lower_finish == upper_finish {
                format_timestamp(lower_finish)
            } else {
                format!(
                    "{}–{}",
                    format_timestamp(lower_finish),
                    format_timestamp(upper_finish)
                )
            }
        }
        (Some(lower), None) if generated_at > 0 && lower >= 0 => generated_at
            .checked_add(lower)
            .map(|finish| format!("≥{}", format_timestamp(finish)))
            .unwrap_or_else(|| "unknown".to_string()),
        (None, Some(upper)) if generated_at > 0 && upper >= 0 => generated_at
            .checked_add(upper)
            .map(|finish| format!("≤{}", format_timestamp(finish)))
            .unwrap_or_else(|| "unknown".to_string()),
        _ => overall
            .finish_at
            .filter(|finish| *finish > 0)
            .map(|finish| format!("≤{}", format_timestamp(finish)))
            .unwrap_or_else(|| "unknown".to_string()),
    }
}
