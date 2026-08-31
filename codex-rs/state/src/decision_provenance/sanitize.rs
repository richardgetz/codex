//! Privacy sanitization for decision provenance events.

use super::model::ChangeSet;
use super::model::Crossroad;
use super::model::Decision;
use super::model::PreferenceBoundary;
use super::model::PrivacyClass;
use super::model::ScopeRef;
use super::model::SourceReference;
use super::model::Warrant;
use super::query::ProvenanceEvent;
use super::query::ProvenanceEventPayload;

const REDACTED: &str = "[redacted]";

pub(crate) fn redact_sensitive_text(value: &str) -> String {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    let secret_prefix = ["sk-", "ghp_", "github_pat_"];
    let assignment_marker = [
        "bearer ",
        "password=",
        "secret=",
        "token=",
        "api_key=",
        "apikey=",
    ];
    let has_secret_prefix = secret_prefix.iter().any(|marker| {
        lower.match_indices(marker).any(|(index, _)| {
            index == 0
                || lower[..index]
                    .chars()
                    .next_back()
                    .is_some_and(|character| !character.is_ascii_alphanumeric())
        })
    });
    if has_secret_prefix
        || assignment_marker
            .iter()
            .any(|marker| lower.contains(marker))
    {
        return REDACTED.to_string();
    }
    trimmed.chars().take(4_096).collect()
}

fn sanitize_source(source: &mut SourceReference, parent_privacy: PrivacyClass) {
    source.source_type = redact_sensitive_text(&source.source_type);
    source.reference = redact_sensitive_text(&source.reference);
    source.label = source.label.as_deref().map(redact_sensitive_text);
    if source.privacy == PrivacyClass::Sensitive || parent_privacy == PrivacyClass::Sensitive {
        if parent_privacy == PrivacyClass::Sensitive {
            source.privacy = PrivacyClass::Sensitive;
        }
        source.reference = REDACTED.to_string();
        source.label = Some(REDACTED.to_string());
    }
}

fn sanitize_text(value: &str, privacy: PrivacyClass) -> String {
    if privacy == PrivacyClass::Sensitive {
        REDACTED.to_string()
    } else {
        redact_sensitive_text(value)
    }
}

fn sanitize_optional_text(value: Option<&str>, privacy: PrivacyClass) -> Option<String> {
    value.map(|value| sanitize_text(value, privacy))
}

fn sanitize_texts(values: &mut Vec<String>, privacy: PrivacyClass) {
    for value in values {
        *value = sanitize_text(value, privacy);
    }
}

fn sanitize_scope(scope: &mut ScopeRef, privacy: PrivacyClass) {
    scope.id = sanitize_text(&scope.id, privacy);
}

fn sanitize_private_reference(value: &mut Option<String>, privacy: PrivacyClass) {
    if privacy == PrivacyClass::Sensitive {
        *value = None;
    } else if let Some(value) = value.as_mut() {
        *value = redact_sensitive_text(value);
    }
}

fn sanitize_boundary(boundary: &mut PreferenceBoundary, parent_privacy: PrivacyClass) {
    let privacy = if boundary.privacy == PrivacyClass::Sensitive
        || parent_privacy == PrivacyClass::Sensitive
    {
        boundary.privacy = PrivacyClass::Sensitive;
        PrivacyClass::Sensitive
    } else {
        boundary.privacy
    };
    sanitize_scope(&mut boundary.scope, privacy);
    boundary.statement = sanitize_text(&boundary.statement, privacy);
    boundary.rationale = sanitize_optional_text(boundary.rationale.as_deref(), privacy);
    sanitize_source(&mut boundary.source, privacy);
    if privacy == PrivacyClass::Sensitive {
        boundary.statement = REDACTED.to_string();
        boundary.rationale = Some(REDACTED.to_string());
    }
}

fn sanitize_crossroad(crossroad: &mut Crossroad, parent_privacy: PrivacyClass) {
    let privacy = if crossroad.privacy == PrivacyClass::Sensitive
        || parent_privacy == PrivacyClass::Sensitive
    {
        crossroad.privacy = PrivacyClass::Sensitive;
        PrivacyClass::Sensitive
    } else {
        crossroad.privacy
    };
    sanitize_private_reference(&mut crossroad.request_ref, privacy);
    sanitize_private_reference(&mut crossroad.task_ref, privacy);
    sanitize_private_reference(&mut crossroad.project_ref, privacy);
    sanitize_private_reference(&mut crossroad.session_id, privacy);
    sanitize_private_reference(&mut crossroad.linked_scratchpad_wait_id, privacy);
    crossroad.question = sanitize_text(&crossroad.question, privacy);
    crossroad.recommended_option = crossroad
        .recommended_option
        .as_deref()
        .map(|value| sanitize_text(value, privacy));
    sanitize_texts(&mut crossroad.expected_tradeoffs, privacy);
    for option in &mut crossroad.options {
        option.label = sanitize_text(&option.label, privacy);
        option.summary = sanitize_optional_text(option.summary.as_deref(), privacy);
        sanitize_texts(&mut option.tradeoffs, privacy);
    }
    for source in &mut crossroad.source_refs {
        sanitize_source(source, privacy);
    }
    if privacy == PrivacyClass::Sensitive {
        crossroad.question = REDACTED.to_string();
    }
}

fn sanitize_decision(decision: &mut Decision, parent_privacy: PrivacyClass) {
    let privacy = if decision.privacy == PrivacyClass::Sensitive
        || parent_privacy == PrivacyClass::Sensitive
    {
        decision.privacy = PrivacyClass::Sensitive;
        PrivacyClass::Sensitive
    } else {
        decision.privacy
    };
    sanitize_private_reference(&mut decision.request_ref, privacy);
    sanitize_private_reference(&mut decision.task_ref, privacy);
    sanitize_private_reference(&mut decision.project_ref, privacy);
    sanitize_private_reference(&mut decision.repository, privacy);
    sanitize_private_reference(&mut decision.source_session_id, privacy);
    sanitize_private_reference(&mut decision.source_turn_id, privacy);
    decision.selected_option = sanitize_text(&decision.selected_option, privacy);
    sanitize_texts(&mut decision.unselected_options, privacy);
    decision.summary = sanitize_text(&decision.summary, privacy);
    decision.rationale = sanitize_optional_text(decision.rationale.as_deref(), privacy);
    sanitize_texts(&mut decision.assumptions, privacy);
    sanitize_texts(&mut decision.tradeoffs, privacy);
    if privacy == PrivacyClass::Sensitive {
        decision.summary = REDACTED.to_string();
        decision.rationale = Some(REDACTED.to_string());
    }
}

fn sanitize_warrant(warrant: &mut Warrant, parent_privacy: PrivacyClass) {
    let privacy = if warrant.privacy == PrivacyClass::Sensitive
        || parent_privacy == PrivacyClass::Sensitive
    {
        warrant.privacy = PrivacyClass::Sensitive;
        PrivacyClass::Sensitive
    } else {
        warrant.privacy
    };
    sanitize_texts(&mut warrant.observations, privacy);
    sanitize_texts(&mut warrant.assumptions, privacy);
    sanitize_texts(&mut warrant.priorities, privacy);
    sanitize_texts(&mut warrant.tradeoffs, privacy);
    warrant.uncertainty = sanitize_optional_text(warrant.uncertainty.as_deref(), privacy);
    warrant.qualifier = sanitize_optional_text(warrant.qualifier.as_deref(), privacy);
    for source in &mut warrant.evidence_refs {
        sanitize_source(source, privacy);
    }
}

fn sanitize_change_set(change_set: &mut ChangeSet, parent_privacy: PrivacyClass) {
    let privacy = if change_set.privacy == PrivacyClass::Sensitive
        || parent_privacy == PrivacyClass::Sensitive
    {
        change_set.privacy = PrivacyClass::Sensitive;
        PrivacyClass::Sensitive
    } else {
        change_set.privacy
    };
    sanitize_private_reference(&mut change_set.session_id, privacy);
    sanitize_private_reference(&mut change_set.scratchpad_id, privacy);
    sanitize_texts(&mut change_set.paths, privacy);
    sanitize_texts(&mut change_set.test_runs, privacy);
    change_set.commit_sha = sanitize_optional_text(change_set.commit_sha.as_deref(), privacy);
    change_set.git_intent_note_ref =
        sanitize_optional_text(change_set.git_intent_note_ref.as_deref(), privacy);
    change_set.pull_request = sanitize_optional_text(change_set.pull_request.as_deref(), privacy);
    change_set.issue = sanitize_optional_text(change_set.issue.as_deref(), privacy);
    change_set.deployment_result =
        sanitize_optional_text(change_set.deployment_result.as_deref(), privacy);
    change_set.later_failure_or_rollback =
        sanitize_optional_text(change_set.later_failure_or_rollback.as_deref(), privacy);
    for source in &mut change_set.source_refs {
        sanitize_source(source, privacy);
    }
}

pub(crate) fn sanitize_event(event: &mut ProvenanceEvent) {
    if payload_is_sensitive(&event.payload) {
        event.privacy = PrivacyClass::Sensitive;
    }
    match &mut event.payload {
        ProvenanceEventPayload::PreferenceBoundary(boundary) => {
            sanitize_boundary(boundary, event.privacy)
        }
        ProvenanceEventPayload::BoundaryTransition(transition) => {
            if let Some(replacement) = transition.replacement.as_mut() {
                sanitize_boundary(replacement, event.privacy);
            }
            if let Some(source) = transition.source.as_mut() {
                sanitize_source(source, event.privacy);
            }
        }
        ProvenanceEventPayload::Crossroad(crossroad) => {
            sanitize_crossroad(crossroad, event.privacy)
        }
        ProvenanceEventPayload::CrossroadStatus { .. } => {}
        ProvenanceEventPayload::Decision(decision) => sanitize_decision(decision, event.privacy),
        ProvenanceEventPayload::DecisionStatus { .. } => {}
        ProvenanceEventPayload::Warrant(warrant) => sanitize_warrant(warrant, event.privacy),
        ProvenanceEventPayload::ChangeSet(change_set) => {
            sanitize_change_set(change_set, event.privacy)
        }
        ProvenanceEventPayload::Relationship(relationship) => {
            if event.privacy == PrivacyClass::Sensitive {
                relationship.privacy = PrivacyClass::Sensitive;
            }
            relationship.summary = sanitize_optional_text(
                relationship.summary.as_deref(),
                if relationship.privacy == PrivacyClass::Sensitive {
                    PrivacyClass::Sensitive
                } else {
                    event.privacy
                },
            );
            for source in &mut relationship.source_refs {
                sanitize_source(source, event.privacy);
            }
        }
        ProvenanceEventPayload::Notification(notification) => {
            if event.privacy == PrivacyClass::Sensitive {
                notification.privacy = PrivacyClass::Sensitive;
            }
            let privacy = if notification.privacy == PrivacyClass::Sensitive {
                PrivacyClass::Sensitive
            } else {
                event.privacy
            };
            notification.message = sanitize_text(&notification.message, privacy);
            notification.choice = sanitize_optional_text(notification.choice.as_deref(), privacy);
            for source in &mut notification.source_refs {
                sanitize_source(source, privacy);
            }
        }
    }
}

fn payload_is_sensitive(payload: &ProvenanceEventPayload) -> bool {
    match payload {
        ProvenanceEventPayload::PreferenceBoundary(boundary) => {
            boundary.privacy == PrivacyClass::Sensitive
                || boundary.source.privacy == PrivacyClass::Sensitive
        }
        ProvenanceEventPayload::BoundaryTransition(transition) => {
            transition
                .replacement
                .as_ref()
                .is_some_and(|replacement| replacement.privacy == PrivacyClass::Sensitive)
                || transition
                    .source
                    .as_ref()
                    .is_some_and(|source| source.privacy == PrivacyClass::Sensitive)
        }
        ProvenanceEventPayload::Crossroad(crossroad) => {
            crossroad.privacy == PrivacyClass::Sensitive
                || crossroad
                    .source_refs
                    .iter()
                    .any(|source| source.privacy == PrivacyClass::Sensitive)
        }
        ProvenanceEventPayload::CrossroadStatus { .. } => false,
        ProvenanceEventPayload::Decision(decision) => decision.privacy == PrivacyClass::Sensitive,
        ProvenanceEventPayload::DecisionStatus { .. } => false,
        ProvenanceEventPayload::Warrant(warrant) => {
            warrant.privacy == PrivacyClass::Sensitive
                || warrant
                    .evidence_refs
                    .iter()
                    .any(|source| source.privacy == PrivacyClass::Sensitive)
        }
        ProvenanceEventPayload::ChangeSet(change_set) => {
            change_set.privacy == PrivacyClass::Sensitive
                || change_set
                    .source_refs
                    .iter()
                    .any(|source| source.privacy == PrivacyClass::Sensitive)
        }
        ProvenanceEventPayload::Relationship(relationship) => {
            relationship.privacy == PrivacyClass::Sensitive
                || relationship
                    .source_refs
                    .iter()
                    .any(|source| source.privacy == PrivacyClass::Sensitive)
        }
        ProvenanceEventPayload::Notification(notification) => {
            notification.privacy == PrivacyClass::Sensitive
                || notification
                    .source_refs
                    .iter()
                    .any(|source| source.privacy == PrivacyClass::Sensitive)
        }
    }
}
