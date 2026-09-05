//! Bounded plain-text rendering for decision-provenance TUI commands.

use codex_state::decision_provenance::Decision;
use codex_state::decision_provenance::DecisionWhy;
use codex_state::decision_provenance::EventSummary;
use codex_state::decision_provenance::PreferenceBoundary;
use codex_state::decision_provenance::ProvenanceRelationship;
use codex_state::decision_provenance::ScopeRef;
use std::fmt::Write;

pub(super) fn format_decisions(decisions: Vec<Decision>) -> anyhow::Result<String> {
    if decisions.is_empty() {
        return Ok("No decisions found.".to_string());
    }
    let mut output = String::from("Decisions:\n");
    for decision in decisions {
        writeln!(
            output,
            "- {} [{}] {} — {} ({}, {})",
            decision.id,
            decision.status.as_str(),
            decision.selected_option,
            decision.summary,
            decision.actor.as_str(),
            decision.approval_state.as_str()
        )?;
    }
    Ok(output)
}

pub(super) fn format_crossroads(
    crossroads: Vec<codex_state::decision_provenance::Crossroad>,
) -> anyhow::Result<String> {
    if crossroads.is_empty() {
        return Ok("No crossroads found.".to_string());
    }
    let mut output = String::from("Crossroads (informational; nothing here blocks work):\n");
    for crossroad in crossroads {
        writeln!(
            output,
            "- {} [{}] — {} ({} option{}, {} source{})",
            crossroad.id,
            display_crossroad_status(crossroad.status),
            crossroad.question,
            crossroad.options.len(),
            if crossroad.options.len() == 1 {
                ""
            } else {
                "s"
            },
            crossroad.source_refs.len(),
            if crossroad.source_refs.len() == 1 {
                ""
            } else {
                "s"
            },
        )?;
    }
    output.push_str(
        "Entry point: `/decisions crossroads [all]`. Use full or unique short IDs; ambiguous prefixes are rejected.\nReview bookkeeping: `/decisions reviewed <id>`, `/decisions dismiss <id>`, or `/decisions revisit <id>`; these never approve, block, or roll back work.\nUse `/decisions show <id>` to discuss sources and linked history.\n",
    );
    Ok(output)
}

pub(super) fn format_crossroad_detail(
    crossroad: &codex_state::decision_provenance::Crossroad,
    relationships: &[ProvenanceRelationship],
    linked_decisions: &[Decision],
    history: &[EventSummary],
) -> String {
    let mut output = format!(
        "Crossroad `{}`\nStatus: {}\nQuestion: {}\n\nThis record is informational bookkeeping. It does not approve a path, block execution, or roll back code.",
        crossroad.id,
        display_crossroad_status(crossroad.status),
        crossroad.question
    );
    let _ = write!(
        output,
        "\n\nReview bookkeeping: `/decisions reviewed {}`, `/decisions dismiss {}`, or `/decisions revisit {}`. These commands only append review history; they do not approve, block, or roll back work.\nUse a full ID or a unique short prefix; ambiguous prefixes must be disambiguated.",
        crossroad.id, crossroad.id, crossroad.id
    );
    if !crossroad.options.is_empty() {
        output.push_str("\n\nOptions recorded for discussion/reference:");
    }
    for option in &crossroad.options {
        let _ = write!(
            output,
            "\n- `{}`: {} — {}",
            option.id,
            option.label,
            option.summary.as_deref().unwrap_or("no summary recorded")
        );
        for tradeoff in &option.tradeoffs {
            let _ = write!(output, "\n  Tradeoff: {tradeoff}");
        }
    }
    if !crossroad.expected_tradeoffs.is_empty() {
        let _ = write!(
            output,
            "\n\nRecorded caveats: {}",
            crossroad.expected_tradeoffs.join("; ")
        );
    }
    if crossroad.privacy == codex_state::decision_provenance::PrivacyClass::Sensitive {
        output.push_str("\n\nPrior sources: (sensitive references withheld)");
    } else if !crossroad.source_refs.is_empty() {
        output.push_str("\n\nPrior sources (references only):");
        for source in &crossroad.source_refs {
            let _ = write!(output, "\n- {}:{}", source.source_type, source.reference);
        }
    }
    if !relationships.is_empty() {
        output.push_str("\n\nLinked records:");
        for relationship in relationships {
            if relationship.from_type == codex_state::decision_provenance::EntityType::Crossroad
                && relationship.from_id == crossroad.id
                && relationship.to_type == codex_state::decision_provenance::EntityType::Crossroad
                && relationship.to_id == crossroad.id
            {
                continue;
            }
            let (arrow, endpoint_type, endpoint_id) = if relationship.from_type
                == codex_state::decision_provenance::EntityType::Crossroad
                && relationship.from_id == crossroad.id
            {
                ("→", relationship.to_type, relationship.to_id.as_str())
            } else if relationship.to_type
                == codex_state::decision_provenance::EntityType::Crossroad
                && relationship.to_id == crossroad.id
            {
                ("←", relationship.from_type, relationship.from_id.as_str())
            } else {
                ("→", relationship.to_type, relationship.to_id.as_str())
            };
            let _ = write!(
                output,
                "\n- {} {arrow} {}:{} [{}]{}",
                relationship.relation.as_str(),
                endpoint_type.as_str(),
                endpoint_id,
                relationship.evidence.as_str(),
                relationship
                    .summary
                    .as_deref()
                    .map(|summary| format!(" — {summary}"))
                    .unwrap_or_default()
            );
        }
    }
    if !linked_decisions.is_empty() {
        output.push_str("\n\nLinked decisions:");
        for decision in linked_decisions {
            let _ = write!(
                output,
                "\n- `{}` [{}] selected `{}` by {} — {}",
                decision.id,
                decision.status.as_str(),
                decision.selected_option,
                decision.actor.as_str(),
                decision.summary
            );
        }
    }
    if !history.is_empty() {
        output.push_str("\n\nHistory:");
        for event in history {
            let _ = write!(
                output,
                "\n- {} {} by {} ({})",
                event.occurred_at.to_rfc3339(),
                event.event_type,
                event.actor.as_str(),
                event.event_id
            );
        }
    }
    output
}

pub(super) fn format_boundaries(boundaries: Vec<PreferenceBoundary>) -> anyhow::Result<String> {
    if boundaries.is_empty() {
        return Ok("No preference boundaries found.".to_string());
    }
    let mut output = String::from("Preference boundaries:\n");
    for boundary in boundaries {
        writeln!(
            output,
            "- {} [{} / {}] {} — {}",
            boundary.id,
            boundary.kind.as_str(),
            boundary.lifecycle_status.as_str(),
            format_scope(&boundary.scope),
            boundary.statement
        )?;
    }
    Ok(output)
}

pub(super) fn format_decision(decision: &Decision) -> String {
    format!(
        "Decision `{}`\nStatus: {}\nSelected: {}\nActor: {}\nApproval: {}\nAuthority: {}\nSummary: {}\nRationale: {}",
        decision.id,
        decision.status.as_str(),
        decision.selected_option,
        decision.actor.as_str(),
        decision.approval_state.as_str(),
        decision.authority_basis.as_str(),
        decision.summary,
        decision.rationale.as_deref().unwrap_or("(not recorded)")
    )
}

pub(super) fn format_boundary(boundary: &PreferenceBoundary) -> String {
    format!(
        "Preference boundary `{}`\nKind: {}\nStatus: {}\nScope: {}\nStrength: {}\nAuthority: {}\nStatement: {}\nRationale: {}\nSource: {}",
        boundary.id,
        boundary.kind.as_str(),
        boundary.lifecycle_status.as_str(),
        format_scope(&boundary.scope),
        boundary.strength.as_str(),
        boundary.authority.as_str(),
        boundary.statement,
        boundary.rationale.as_deref().unwrap_or("(not recorded)"),
        format_source(&boundary.source),
    )
}

fn format_scope(scope: &ScopeRef) -> String {
    format!("{}:{}", scope.kind.as_str(), scope.id)
}

fn display_crossroad_status(
    status: codex_state::decision_provenance::CrossroadStatus,
) -> &'static str {
    match status {
        codex_state::decision_provenance::CrossroadStatus::Open => "open",
        codex_state::decision_provenance::CrossroadStatus::Resolved => "reviewed",
        codex_state::decision_provenance::CrossroadStatus::Cancelled => "dismissed",
        codex_state::decision_provenance::CrossroadStatus::Reopened => "reopened",
    }
}

pub(super) fn format_decision_why(why: &DecisionWhy) -> String {
    let mut output = format_decision(&why.decision);
    output.push_str("\n\nWhy:");
    if let Some(crossroad) = &why.crossroad {
        output.push_str(&format!(
            "\nCrossroad: {} — {}",
            crossroad.id, crossroad.question
        ));
        if !crossroad.expected_tradeoffs.is_empty() {
            output.push_str(&format!(
                "\nExpected tradeoffs: {}",
                crossroad.expected_tradeoffs.join("; ")
            ));
        }
    }
    if !why.boundaries.is_empty() {
        output.push_str("\nPreference boundaries:");
        for boundary in &why.boundaries {
            output.push_str(&format!(
                "\n- {} [{}] {}",
                boundary.id,
                boundary.lifecycle_status.as_str(),
                boundary.statement
            ));
        }
    }
    if let Some(warrant) = &why.warrant {
        output.push_str(&format!(
            "\nObservations: {}",
            warrant.observations.join("; ")
        ));
        output.push_str(&format!(
            "\nAssumptions: {}",
            warrant.assumptions.join("; ")
        ));
        if let Some(uncertainty) = &warrant.uncertainty {
            output.push_str(&format!("\nUncertainty: {uncertainty}"));
        }
    }
    if !why.decision.tradeoffs.is_empty() {
        output.push_str(&format!(
            "\nTradeoffs: {}",
            why.decision.tradeoffs.join("; ")
        ));
    }
    if !why.change_sets.is_empty() {
        output.push_str("\nChange sets:");
        for change_set in &why.change_sets {
            output.push_str(&format!("\n- {}", format_change_set(change_set)));
        }
    }
    if !why.relationships.is_empty() {
        output.push_str("\nRelationships:");
        for relationship in &why.relationships {
            output.push_str(&format!(
                "\n- {} {} {} ({})",
                relationship.from_id,
                relationship.relation.as_str(),
                relationship.to_id,
                relationship.evidence.as_str()
            ));
        }
    }
    if !why.history.is_empty() {
        output.push_str("\nHistory:");
        for event in &why.history {
            output.push_str(&format!(
                "\n- {} {} by {}",
                event.occurred_at.to_rfc3339(),
                event.event_type,
                event.actor.as_str()
            ));
        }
    }
    output
}

pub(super) fn format_change_sets(
    change_sets: Vec<codex_state::decision_provenance::ChangeSet>,
) -> anyhow::Result<String> {
    let mut output = String::from("Change sets:\n");
    for change_set in change_sets {
        writeln!(output, "- {}", format_change_set(&change_set))?;
    }
    Ok(output)
}

fn format_change_set(change_set: &codex_state::decision_provenance::ChangeSet) -> String {
    let mut parts = vec![change_set.id.clone()];
    if let Some(commit_sha) = &change_set.commit_sha {
        parts.push(format!("commit {commit_sha}"));
    }
    if let Some(pull_request) = &change_set.pull_request {
        parts.push(format!("PR {pull_request}"));
    }
    if let Some(scratchpad_id) = &change_set.scratchpad_id {
        parts.push(format!("scratchpad {scratchpad_id}"));
    }
    parts.join(" — ")
}

pub(super) fn format_id_list(title: &str, values: Vec<String>) -> String {
    let mut output = format!("{title}:\n");
    for value in values {
        output.push_str("- ");
        output.push_str(&value);
        output.push('\n');
    }
    output
}

fn format_source(source: &codex_state::decision_provenance::SourceReference) -> String {
    format!("{}:{}", source.source_type, source.reference)
}
