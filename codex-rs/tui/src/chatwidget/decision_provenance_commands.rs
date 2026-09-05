//! TUI traversal for the local decision-provenance store.

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::history_cell::PlainHistoryCell;
use codex_protocol::ThreadId;
use codex_rollout::StateDbHandle;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::CrossroadFilter;
use codex_state::decision_provenance::CrossroadStatus;
use codex_state::decision_provenance::Decision;
use codex_state::decision_provenance::DecisionFilter;
use codex_state::decision_provenance::DecisionStatus;
use codex_state::decision_provenance::EntityType;
use codex_state::decision_provenance::ProvenanceWriteOptions;
use codex_state::decision_provenance::now;
use ratatui::text::Line;
use uuid::Uuid;

#[path = "decision_provenance_boundary_commands.rs"]
mod boundary_commands;
#[path = "decision_provenance_format.rs"]
mod format;
#[path = "decision_provenance_resolver.rs"]
mod resolver;
use boundary_commands::run_boundaries_command;
use format::format_boundaries;
use format::format_boundary;
use format::format_change_sets;
use format::format_crossroad_detail;
use format::format_crossroads;
use format::format_decision_why;
use format::format_decisions;
use format::format_id_list;
use resolver::ShowTarget;
use resolver::resolve_crossroad;
use resolver::resolve_decision_id;
use resolver::resolve_show_target;

const DECISIONS_USAGE: &str = "Usage: /decisions [list|crossroads [all]|show <id>|why <id> [--at <timestamp>]|history <id>|search <text>|influenced-by <boundary-id>|sessions <id>|artifacts <id>|reviewed <crossroad-id>|dismiss <crossroad-id>|resolve <id>|revisit <id>|reopen <id>|override <id> (deprecated alias)]";
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
        "crossroads" => {
            let crossroads = match rest {
                "" | "open" => state_db.list_open_crossroads(20).await?,
                "all" => state_db.list_crossroads(CrossroadFilter::default()).await?,
                _ => return Err(anyhow::anyhow!(DECISIONS_USAGE)),
            };
            format_crossroads(crossroads)
        }
        "show" => {
            let requested_id = required_id(rest, DECISIONS_USAGE)?;
            match resolve_show_target(state_db, requested_id).await? {
                ShowTarget::Crossroad(crossroad) => {
                    let relationships = state_db
                        .relationships_for(EntityType::Crossroad, &crossroad.id)
                        .await?;
                    let mut linked_decisions = Vec::new();
                    for relationship in &relationships {
                        let decision_id = if relationship.to_type == EntityType::Decision {
                            Some(relationship.to_id.as_str())
                        } else if relationship.from_type == EntityType::Decision {
                            Some(relationship.from_id.as_str())
                        } else {
                            None
                        };
                        if let Some(decision_id) = decision_id
                            && let Some(decision) = state_db.get_decision(decision_id).await?
                            && !linked_decisions
                                .iter()
                                .any(|existing: &Decision| existing.id == decision.id)
                        {
                            linked_decisions.push(decision);
                        }
                    }
                    let history = state_db.crossroad_history(&crossroad.id).await?;
                    Ok(format_crossroad_detail(
                        &crossroad,
                        &relationships,
                        &linked_decisions,
                        &history,
                    ))
                }
                ShowTarget::Decision(decision) => {
                    let why = state_db.decision_why(&decision.id).await?.ok_or_else(|| {
                        anyhow::anyhow!("decision `{}` was not found", decision.id)
                    })?;
                    Ok(format_decision_why(&why))
                }
            }
        }
        "why" => {
            let (requested_id, at) = decision_id_and_timestamp(rest)?;
            let id = resolve_decision_id(state_db, requested_id).await?;
            let why = match at {
                Some(at) => state_db
                    .decision_why_at(&id, at)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("decision `{id}` was not found"))?,
                None => state_db
                    .decision_why(&id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("decision `{id}` was not found"))?,
            };
            Ok(format_decision_why(&why))
        }
        "history" => {
            let requested_id = required_id(rest, DECISIONS_USAGE)?;
            match resolve_show_target(state_db, requested_id).await? {
                ShowTarget::Crossroad(crossroad) => {
                    let history = state_db.crossroad_history(&crossroad.id).await?;
                    Ok(format_event_history("crossroad", &crossroad.id, history))
                }
                ShowTarget::Decision(decision) => {
                    let history = state_db.decision_history(&decision.id).await?;
                    Ok(format_event_history("decision", &decision.id, history))
                }
            }
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
            let requested_id = required_id(rest, DECISIONS_USAGE)?;
            let id = resolve_decision_id(state_db, requested_id).await?;
            let sessions = state_db.decision_sessions(&id).await?;
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
            let requested_id = required_id(rest, DECISIONS_USAGE)?;
            let id = resolve_decision_id(state_db, requested_id).await?;
            let artifacts = state_db.decision_artifacts(&id).await?;
            if artifacts.is_empty() {
                return Err(anyhow::anyhow!("decision `{id}` has no linked artifacts"));
            }
            format_change_sets(artifacts)
        }
        "reviewed" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            transition_crossroad(state_db, id, CrossroadStatus::Resolved, "reviewed").await
        }
        "dismiss" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            transition_crossroad(state_db, id, CrossroadStatus::Cancelled, "dismissed").await
        }
        "resolve" => {
            transition_decision_or_crossroad(
                state_db,
                required_id(rest, DECISIONS_USAGE)?,
                CrossroadStatus::Resolved,
                DecisionStatus::Accepted,
                "reviewed",
            )
            .await
        }
        "revisit" | "reopen" => {
            transition_decision_or_crossroad(
                state_db,
                required_id(rest, DECISIONS_USAGE)?,
                CrossroadStatus::Reopened,
                DecisionStatus::Reopened,
                "reopened",
            )
            .await
        }
        "override" => {
            let id = required_id(rest, DECISIONS_USAGE)?;
            match resolve_show_target(state_db, id).await? {
                ShowTarget::Crossroad(_) => Ok(format!(
                    "`/decisions override {id}` is deprecated; use `/decisions revisit {id}`. Reopening is bookkeeping only and does not grant approval or change execution."
                )),
                ShowTarget::Decision(decision) => {
                    if decision.status == DecisionStatus::Reopened {
                        return Ok(format!(
                            "Decision `{}` is already reopened. This legacy alias does not grant approval; record any replacement with an explicit actor and source.",
                            decision.id
                        ));
                    }
                    state_db
                        .transition_decision(
                            &decision.id,
                            DecisionStatus::Reopened,
                            user_write_options("override", &decision.id),
                        )
                        .await?;
                    Ok(format!(
                        "Decision `{}` reopened by the deprecated alias. This is bookkeeping only; it does not grant approval or change execution.",
                        decision.id
                    ))
                }
            }
        }
        _ => Err(anyhow::anyhow!(DECISIONS_USAGE)),
    }
}

async fn transition_decision_or_crossroad(
    state_db: &codex_state::StateRuntime,
    requested_id: &str,
    crossroad_status: CrossroadStatus,
    decision_status: DecisionStatus,
    crossroad_label: &str,
) -> anyhow::Result<String> {
    match resolve_show_target(state_db, requested_id).await? {
        ShowTarget::Crossroad(crossroad) => {
            if crossroad.status == crossroad_status {
                return Ok(format!(
                    "Crossroad `{}` is already {}.",
                    crossroad.id, crossroad_label
                ));
            }
            state_db
                .transition_crossroad(
                    &crossroad.id,
                    crossroad_status,
                    user_write_options(crossroad_label, &crossroad.id),
                )
                .await?;
            Ok(format!(
                "Crossroad `{}` marked {}. This only records review bookkeeping; it does not approve a path or change execution.",
                crossroad.id, crossroad_label
            ))
        }
        ShowTarget::Decision(decision) => {
            if decision.status == decision_status {
                return Ok(format!(
                    "Decision `{}` is already {}.",
                    decision.id,
                    decision_status.as_str()
                ));
            }
            state_db
                .transition_decision(
                    &decision.id,
                    decision_status,
                    user_write_options(decision_status.as_str(), &decision.id),
                )
                .await?;
            Ok(format!(
                "Decision `{}` is now {}.",
                decision.id,
                decision_status.as_str()
            ))
        }
    }
}

async fn transition_crossroad(
    state_db: &codex_state::StateRuntime,
    requested_id: &str,
    status: CrossroadStatus,
    label: &str,
) -> anyhow::Result<String> {
    let Some(crossroad) = resolve_crossroad(state_db, requested_id).await? else {
        return Err(anyhow::anyhow!("crossroad `{requested_id}` was not found"));
    };
    if crossroad.status == status {
        return Ok(format!("Crossroad `{}` is already {label}.", crossroad.id));
    }
    state_db
        .transition_crossroad(
            &crossroad.id,
            status,
            user_write_options(label, &crossroad.id),
        )
        .await?;
    Ok(format!(
        "Crossroad `{}` marked {label}. This only records bookkeeping; it does not approve a path, block execution, or roll back code.",
        crossroad.id
    ))
}

fn format_event_history(
    entity_type: &str,
    id: &str,
    history: Vec<codex_state::decision_provenance::EventSummary>,
) -> String {
    let mut output = format!("History for {entity_type} `{id}`:\n");
    if history.is_empty() {
        output.push_str("(no events recorded)\n");
    }
    for event in history {
        output.push_str(&format!(
            "- {} {} by {} ({})\n",
            event.occurred_at.to_rfc3339(),
            event.event_type,
            event.actor.as_str(),
            event.event_id
        ));
    }
    output
}

fn user_write_options(action: &str, id: &str) -> ProvenanceWriteOptions {
    ProvenanceWriteOptions {
        idempotency_key: Some(format!("tui:{action}:{id}:{}", Uuid::new_v4())),
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
