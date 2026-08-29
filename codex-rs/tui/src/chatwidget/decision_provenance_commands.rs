//! TUI traversal for the local decision-provenance store.

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::history_cell::PlainHistoryCell;
use codex_protocol::ThreadId;
use codex_rollout::StateDbHandle;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::BoundaryTransition;
use codex_state::decision_provenance::BoundaryTransitionKind;
use codex_state::decision_provenance::CrossroadStatus;
use codex_state::decision_provenance::DecisionFilter;
use codex_state::decision_provenance::DecisionStatus;
use codex_state::decision_provenance::LifecycleStatus;
use codex_state::decision_provenance::PreferenceBoundaryFilter;
use codex_state::decision_provenance::PreferenceKind;
use codex_state::decision_provenance::PreferenceStrength;
use codex_state::decision_provenance::ProvenanceWriteOptions;
use codex_state::decision_provenance::SourceReference;
use codex_state::decision_provenance::Timestamps;
use codex_state::decision_provenance::now;
use ratatui::text::Line;
use std::fmt::Write;
use uuid::Uuid;

#[path = "decision_provenance_format.rs"]
mod format;
use format::format_boundaries;
use format::format_boundary;
use format::format_change_sets;
use format::format_crossroads;
use format::format_decision;
use format::format_decision_why;
use format::format_decisions;
use format::format_id_list;

const DECISIONS_USAGE: &str = "Usage: /decisions [list|crossroads|show <id>|why <id> [--at <timestamp>]|history <id>|search <text>|influenced-by <boundary-id>|sessions <id>|artifacts <id>|resolve <id>|revisit <id>|reopen <id>|override <id>]";
const PREFERENCE_BOUNDARIES_USAGE: &str = "Usage: /preference-boundaries [list|show <id>|search <text>|decisions <id>|confirm <id>|narrow <id> <statement>|broaden <id> <statement>|withdraw <id>]";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandFamily {
    Decisions,
    PreferenceBoundaries,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProvenanceCommandAccess {
    Local,
    RemoteUnavailable,
}

/// Run one provenance command off the UI loop and insert its bounded text result into history.
pub(crate) fn spawn_command(
    access: ProvenanceCommandAccess,
    state_db: Option<StateDbHandle>,
    family: CommandFamily,
    args: &str,
    thread_id: Option<ThreadId>,
    app_event_tx: AppEventSender,
) {
    let args = args.trim().to_string();
    tokio::spawn(async move {
        let result = match access {
            ProvenanceCommandAccess::RemoteUnavailable => Err(anyhow::anyhow!(
                "decision provenance commands are unavailable for a remote app-server session"
            )),
            ProvenanceCommandAccess::Local => match state_db {
                Some(state_db) => run_command(state_db.as_ref(), family, &args).await,
                None => Err(anyhow::anyhow!(
                    "local state is unavailable; decision provenance cannot be queried in this session"
                )),
            },
        };
        let text = result.unwrap_or_else(|err| format!("Decision provenance error: {err:#}"));
        let lines = text
            .lines()
            .map(|line| Line::from(line.to_string()))
            .collect();
        let cell = Box::new(PlainHistoryCell::new(lines));
        match thread_id {
            Some(thread_id) => {
                app_event_tx.send(AppEvent::InsertHistoryCellForThread { thread_id, cell })
            }
            None => app_event_tx.send(AppEvent::InsertHistoryCell(cell)),
        }
    });
}

async fn run_command(
    state_db: &codex_state::StateRuntime,
    family: CommandFamily,
    args: &str,
) -> anyhow::Result<String> {
    match family {
        CommandFamily::Decisions => run_decisions_command(state_db, args).await,
        CommandFamily::PreferenceBoundaries => run_boundaries_command(state_db, args).await,
    }
}

async fn run_decisions_command(
    state_db: &codex_state::StateRuntime,
    args: &str,
) -> anyhow::Result<String> {
    let (subcommand, rest) = first_word(args);
    match subcommand {
        "" | "list" => format_decisions(state_db.list_decisions(DecisionFilter::default()).await?),
        "crossroads" => format_crossroads(state_db.list_open_crossroads(20).await?),
        "show" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            let decision = state_db
                .get_decision(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("decision `{id}` was not found"))?;
            Ok(format_decision(&decision))
        }
        "why" => {
            let (id, at) = decision_id_and_timestamp(rest)?;
            let why = match at {
                Some(at) => state_db
                    .decision_why_at(id, at)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("decision `{id}` was not found"))?,
                None => state_db
                    .decision_why(id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("decision `{id}` was not found"))?,
            };
            Ok(format_decision_why(&why))
        }
        "history" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            let history = state_db.decision_history(id).await?;
            if history.is_empty() {
                return Err(anyhow::anyhow!("decision `{id}` was not found"));
            }
            let mut output = format!("History for decision `{id}`:\n");
            for event in history {
                writeln!(
                    output,
                    "- {} {} by {} ({})",
                    event.occurred_at.to_rfc3339(),
                    event.event_type,
                    event.actor.as_str(),
                    event.event_id
                )?;
            }
            Ok(output)
        }
        "search" => {
            let text = required_text(rest, DECISIONS_USAGE)?;
            format_decisions(
                state_db
                    .list_decisions(DecisionFilter {
                        text: Some(text.to_string()),
                        ..DecisionFilter::default()
                    })
                    .await?,
            )
        }
        "influenced-by" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            format_decisions(state_db.decisions_influenced_by(id, 20).await?)
        }
        "sessions" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            if state_db.get_decision(id).await?.is_none() {
                return Err(anyhow::anyhow!("decision `{id}` was not found"));
            }
            let sessions = state_db.decision_sessions(id).await?;
            if sessions.is_empty() {
                return Ok(format_id_list(
                    &format!("Sessions for decision `{id}`"),
                    Vec::new(),
                ));
            }
            Ok(format_id_list(
                &format!("Sessions for decision `{id}`"),
                sessions,
            ))
        }
        "artifacts" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            let artifacts = state_db.decision_artifacts(id).await?;
            if artifacts.is_empty() {
                return Err(anyhow::anyhow!("decision `{id}` has no linked artifacts"));
            }
            format_change_sets(artifacts)
        }
        "resolve" => {
            transition_decision_or_crossroad(
                state_db,
                required_id(rest, DECISIONS_USAGE)?,
                CrossroadStatus::Resolved,
                DecisionStatus::Accepted,
            )
            .await
        }
        "revisit" | "reopen" => {
            transition_decision_or_crossroad(
                state_db,
                required_id(rest, DECISIONS_USAGE)?,
                CrossroadStatus::Reopened,
                DecisionStatus::Reopened,
            )
            .await
        }
        "override" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            let decision = state_db
                .get_decision(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("decision `{id}` was not found"))?;
            if decision.status == DecisionStatus::Reopened {
                return Ok(format!(
                    "Decision `{id}` is already reopened for an explicit user override."
                ));
            }
            state_db
                .transition_decision(
                    id,
                    DecisionStatus::Reopened,
                    user_write_options("override", id),
                )
                .await?;
            Ok(format!(
                "Decision `{}` reopened for an explicit user override. The prior path remains historical; record the replacement decision with its new scope and rationale.",
                decision.id
            ))
        }
        _ => Err(anyhow::anyhow!(DECISIONS_USAGE)),
    }
}

async fn run_boundaries_command(
    state_db: &codex_state::StateRuntime,
    args: &str,
) -> anyhow::Result<String> {
    let (subcommand, rest) = first_word(args);
    match subcommand {
        "" | "list" => format_boundaries(
            state_db
                .list_preference_boundaries(PreferenceBoundaryFilter::default())
                .await?,
        ),
        "show" => {
            let id = required_id(rest, PREFERENCE_BOUNDARIES_USAGE)?;
            let boundary = state_db
                .get_preference_boundary(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("preference boundary `{id}` was not found"))?;
            Ok(format_boundary(&boundary))
        }
        "search" => {
            let text = required_text(rest, PREFERENCE_BOUNDARIES_USAGE)?;
            format_boundaries(
                state_db
                    .list_preference_boundaries(PreferenceBoundaryFilter {
                        text: Some(text.to_string()),
                        ..PreferenceBoundaryFilter::default()
                    })
                    .await?,
            )
        }
        "decisions" => {
            let id = required_id(rest, PREFERENCE_BOUNDARIES_USAGE)?;
            format_decisions(state_db.decisions_influenced_by(id, 20).await?)
        }
        "confirm" => transition_boundary(state_db, rest, BoundaryTransitionKind::Confirm).await,
        "narrow" => transition_boundary(state_db, rest, BoundaryTransitionKind::Narrow).await,
        "broaden" => transition_boundary(state_db, rest, BoundaryTransitionKind::Broaden).await,
        "withdraw" => transition_boundary(state_db, rest, BoundaryTransitionKind::Withdraw).await,
        _ => Err(anyhow::anyhow!(PREFERENCE_BOUNDARIES_USAGE)),
    }
}

async fn transition_boundary(
    state_db: &codex_state::StateRuntime,
    args: &str,
    transition: BoundaryTransitionKind,
) -> anyhow::Result<String> {
    let (id, statement) = first_word(args);
    if id.is_empty() {
        return Err(anyhow::anyhow!(PREFERENCE_BOUNDARIES_USAGE));
    }
    let replacement_statement = match transition {
        BoundaryTransitionKind::Narrow | BoundaryTransitionKind::Broaden => {
            let statement = statement.trim();
            if statement.is_empty() {
                return Err(anyhow::anyhow!(PREFERENCE_BOUNDARIES_USAGE));
            }
            Some(statement)
        }
        BoundaryTransitionKind::Confirm
        | BoundaryTransitionKind::Activate
        | BoundaryTransitionKind::Withdraw
        | BoundaryTransitionKind::Supersede => {
            if !statement.is_empty() {
                return Err(anyhow::anyhow!(PREFERENCE_BOUNDARIES_USAGE));
            }
            None
        }
    };
    let boundary = state_db
        .get_preference_boundary(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("preference boundary `{id}` was not found"))?;
    if boundary.lifecycle_status == transition.status()
        && !matches!(
            transition,
            BoundaryTransitionKind::Narrow | BoundaryTransitionKind::Broaden
        )
    {
        return Ok(format!(
            "Preference boundary `{}` is already {}.",
            boundary.id,
            transition.status().as_str()
        ));
    }
    if matches!(
        transition,
        BoundaryTransitionKind::Narrow | BoundaryTransitionKind::Broaden
    ) && boundary.lifecycle_status == transition.status()
    {
        let replacement_statement = replacement_statement
            .ok_or_else(|| anyhow::anyhow!("preference boundary pivot statement is required"))?;
        if let Some(replacement_id) = boundary.superseded_by.as_deref()
            && let Some(replacement) = state_db.get_preference_boundary(replacement_id).await?
            && replacement.statement == replacement_statement
        {
            return Ok(format!(
                "Preference boundary `{}` already has the requested {} replacement.",
                boundary.id,
                transition.as_str()
            ));
        }
        anyhow::bail!(
            "preference boundary `{}` already has a {} replacement; use the replacement boundary id for another change",
            boundary.id,
            transition.as_str()
        );
    }
    let replacement = replacement_statement.map(|statement| {
        let mut replacement = boundary.clone();
        replacement.id = deterministic_replacement_id(id, transition, statement);
        replacement.statement = statement.to_string();
        replacement.source = SourceReference::new("tui_user_instruction", format!("boundary:{id}"));
        replacement.kind = PreferenceKind::PreferenceBoundary;
        replacement.strength = PreferenceStrength::Confirmation;
        replacement.lifecycle_status = LifecycleStatus::Confirmed;
        replacement.timestamps = Timestamps::now();
        replacement.authority = Authority::User;
        replacement.confidence = None;
        replacement.superseded_by = None;
        replacement
    });
    state_db
        .transition_preference_boundary(
            BoundaryTransition {
                boundary_id: id.to_string(),
                transition,
                replacement,
                actor: Actor::User,
                source: None,
            },
            user_write_options(transition.as_str(), id),
        )
        .await?;
    Ok(format!(
        "Preference boundary `{}` is now {}. Its prior state remains in history.",
        boundary.id,
        transition.status().as_str()
    ))
}

async fn transition_decision_or_crossroad(
    state_db: &codex_state::StateRuntime,
    id: &str,
    crossroad_status: CrossroadStatus,
    decision_status: DecisionStatus,
) -> anyhow::Result<String> {
    if let Some(crossroad) = state_db.get_crossroad(id).await? {
        if crossroad.status == crossroad_status {
            return Ok(format!(
                "Crossroad `{id}` is already {}.",
                crossroad_status.as_str()
            ));
        }
        state_db
            .transition_crossroad(
                id,
                crossroad_status,
                user_write_options(crossroad_status.as_str(), id),
            )
            .await?;
        return Ok(format!(
            "Crossroad `{id}` is now {}.",
            crossroad_status.as_str()
        ));
    }
    if let Some(decision) = state_db.get_decision(id).await? {
        if decision.status == decision_status {
            return Ok(format!(
                "Decision `{id}` is already {}.",
                decision_status.as_str()
            ));
        }
        state_db
            .transition_decision(
                id,
                decision_status,
                user_write_options(decision_status.as_str(), id),
            )
            .await?;
        return Ok(format!(
            "Decision `{id}` is now {}.",
            decision_status.as_str()
        ));
    }
    Err(anyhow::anyhow!(
        "decision or crossroad `{id}` was not found"
    ))
}

fn deterministic_replacement_id(
    boundary_id: &str,
    transition: BoundaryTransitionKind,
    statement: &str,
) -> String {
    let key = format!(
        "boundary-replacement:{boundary_id}:{}:{statement}",
        transition.as_str()
    );
    format!(
        "boundary_{}",
        Uuid::new_v5(&Uuid::NAMESPACE_OID, key.as_bytes()).simple()
    )
}

fn user_write_options(action: &str, id: &str) -> ProvenanceWriteOptions {
    ProvenanceWriteOptions {
        idempotency_key: Some(format!("tui:{action}:{id}")),
        actor: Actor::User,
        occurred_at: now(),
    }
}

fn first_word(args: &str) -> (&str, &str) {
    let args = args.trim();
    match args.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (args, ""),
    }
}

fn required_id<'a>(args: &'a str, usage: &str) -> anyhow::Result<&'a str> {
    let (id, extra) = first_word(args);
    if id.is_empty() || !extra.is_empty() {
        return Err(anyhow::anyhow!("{usage}"));
    }
    Ok(id)
}

fn required_text<'a>(args: &'a str, usage: &str) -> anyhow::Result<&'a str> {
    let text = args.trim();
    if text.is_empty() {
        return Err(anyhow::anyhow!("{usage}"));
    }
    Ok(text)
}

fn decision_id_and_timestamp(
    args: &str,
) -> anyhow::Result<(&str, Option<chrono::DateTime<chrono::Utc>>)> {
    let (id, rest) = first_word(args);
    if id.is_empty() {
        return Err(anyhow::anyhow!(DECISIONS_USAGE));
    }
    if rest.is_empty() {
        return Ok((id, None));
    }
    let timestamp = rest
        .strip_prefix("--at ")
        .ok_or_else(|| anyhow::anyhow!(DECISIONS_USAGE))?
        .parse::<chrono::DateTime<chrono::FixedOffset>>()
        .map_err(|_| {
            anyhow::anyhow!("invalid timestamp; use RFC3339, for example 2026-01-02T03:04:05Z")
        })?
        .with_timezone(&chrono::Utc);
    Ok((id, Some(timestamp)))
}

#[cfg(test)]
#[path = "decision_provenance_commands_tests.rs"]
mod tests;
