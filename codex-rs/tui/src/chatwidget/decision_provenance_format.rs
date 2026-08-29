//! Bounded plain-text rendering for decision-provenance TUI commands.

use codex_state::decision_provenance::Decision;
use codex_state::decision_provenance::DecisionWhy;
use codex_state::decision_provenance::PreferenceBoundary;
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
        return Ok("No open crossroads found.".to_string());
    }
    let mut output = String::from("Open crossroads:\n");
    for crossroad in crossroads {
        writeln!(
            output,
            "- {} — {} ({} option{})",
            crossroad.id,
            crossroad.question,
            crossroad.options.len(),
            if crossroad.options.len() == 1 {
                ""
            } else {
                "s"
            }
        )?;
    }
    Ok(output)
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
