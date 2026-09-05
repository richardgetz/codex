//! Preference-boundary command handlers for the decision-provenance TUI.

use super::PREFERENCE_BOUNDARIES_USAGE;
use super::first_word;
use super::format_boundaries;
use super::format_boundary;
use super::format_decisions;
use super::required_id;
use super::required_text;
use super::user_write_options;
use codex_state::StateRuntime;
use codex_state::decision_provenance::Actor;
use codex_state::decision_provenance::Authority;
use codex_state::decision_provenance::BoundaryTransition;
use codex_state::decision_provenance::BoundaryTransitionKind;
use codex_state::decision_provenance::LifecycleStatus;
use codex_state::decision_provenance::PreferenceBoundaryFilter;
use codex_state::decision_provenance::PreferenceKind;
use codex_state::decision_provenance::PreferenceStrength;
use codex_state::decision_provenance::SourceReference;
use codex_state::decision_provenance::Timestamps;

pub(super) async fn run_boundaries_command(
    state_db: &StateRuntime,
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
    state_db: &StateRuntime,
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
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, key.as_bytes()).simple()
    )
}
