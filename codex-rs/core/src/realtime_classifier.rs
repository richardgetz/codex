//! Optional model-backed classification for realtime handoffs.
//!
//! The classifier runs before the normal main-agent handoff and returns only a bounded routing
//! decision. It has no tools, receives no thread history, and never emits a user-visible agent
//! turn. Failures are deliberately conservative: the existing text classifier decides instead.

use crate::client_common::Prompt;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::session::Session;
use codex_otel::SessionTelemetry;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::realtime_handoff::RealtimeHandoffClassification;
use codex_protocol::realtime_handoff::RealtimeHandoffClassifier;
use codex_protocol::realtime_handoff::RealtimeHandoffClassifierFallback;
use codex_protocol::realtime_handoff::RealtimeHandoffClassifierKind;
use codex_protocol::realtime_handoff::RealtimeHandoffRouting;
use codex_protocol::realtime_handoff::contains_explicit_mutation_signal;
use codex_rollout_trace::InferenceTraceContext;
use codex_utils_string::take_bytes_at_char_boundary;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;
use tracing::debug;

const CLASSIFIER_TIMEOUT: Duration = Duration::from_secs(3);
const CLASSIFIER_INPUT_MAX_BYTES: usize = 4_096;
const CLASSIFIER_OUTPUT_MAX_BYTES: usize = 1_024;
const CLASSIFIER_INSTRUCTIONS: &str = "You classify one user request before it is sent to a coding agent. Return read_only only when the request is clearly asking for information, explanation, inspection, listing, or status and does not ask the agent to change, run, send, or operate anything. Return substantive for any request that is ambiguous, asks for work, or could cause a side effect. Do not infer missing context. Return only the requested JSON object.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RealtimeHandoffRoutingDecision {
    pub(crate) selected_effort: Option<ReasoningEffort>,
    pub(crate) routing: RealtimeHandoffRouting,
}

#[derive(Debug)]
enum ClassifierFailure {
    InputTooLong,
    InvalidOutput,
    RequestFailed,
    TimedOut,
}

#[derive(Debug, Deserialize)]
struct ClassifierOutput {
    classification: RealtimeHandoffClassification,
}

/// Classify one handoff using the configured model, or the existing text classifier when no model
/// is configured. The returned decision is the only input the main-agent route needs.
pub(crate) async fn classify_realtime_handoff(
    sess: &Session,
    input: &str,
    handoff_id: &str,
) -> RealtimeHandoffRoutingDecision {
    let config = sess.get_config().await;
    let realtime = &config.realtime;
    let Some(configured_effort) = realtime.non_substantive_reasoning_effort.as_ref() else {
        return text_decision(input, None, None);
    };

    let classifier_model = realtime
        .non_substantive_classifier_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    let Some(classifier_model) = classifier_model else {
        return text_decision(input, Some(configured_effort), None);
    };

    let classifier_reasoning_effort = realtime.non_substantive_classifier_reasoning_effort.clone();
    if input.len() > CLASSIFIER_INPUT_MAX_BYTES {
        return text_fallback_decision(
            input,
            configured_effort,
            classifier_model,
            classifier_reasoning_effort,
            ClassifierFailure::InputTooLong,
        );
    }
    let model_result = timeout(
        CLASSIFIER_TIMEOUT,
        classify_with_model(
            sess,
            input,
            handoff_id,
            &classifier_model,
            classifier_reasoning_effort.as_ref(),
        ),
    )
    .await;

    match model_result {
        Ok(Ok(classification)) => model_decision(
            input,
            classification,
            configured_effort,
            classifier_model,
            classifier_reasoning_effort,
        ),
        Ok(Err(failure)) => text_fallback_decision(
            input,
            configured_effort,
            classifier_model,
            classifier_reasoning_effort,
            failure,
        ),
        Err(_) => text_fallback_decision(
            input,
            configured_effort,
            classifier_model,
            classifier_reasoning_effort,
            ClassifierFailure::TimedOut,
        ),
    }
}

fn text_decision(
    input: &str,
    configured_effort: Option<&ReasoningEffort>,
    classifier: Option<RealtimeHandoffClassifier>,
) -> RealtimeHandoffRoutingDecision {
    let read_only = codex_protocol::realtime_handoff::is_conservative_read_only_request(input);
    let selected_effort = read_only.then(|| configured_effort.cloned()).flatten();
    RealtimeHandoffRoutingDecision {
        selected_effort: selected_effort.clone(),
        routing: RealtimeHandoffRouting {
            classifier: classifier.unwrap_or(RealtimeHandoffClassifier {
                kind: RealtimeHandoffClassifierKind::Text,
                model: None,
                reasoning_effort: None,
                fallback: None,
            }),
            classification: if read_only {
                RealtimeHandoffClassification::ReadOnly
            } else {
                RealtimeHandoffClassification::Substantive
            },
            selected_effort,
        },
    }
}

fn model_decision(
    input: &str,
    classification: RealtimeHandoffClassification,
    configured_effort: &ReasoningEffort,
    classifier_model: String,
    classifier_reasoning_effort: Option<ReasoningEffort>,
) -> RealtimeHandoffRoutingDecision {
    let selected_effort = matches!(classification, RealtimeHandoffClassification::ReadOnly)
        .then(|| (!contains_explicit_mutation_signal(input)).then_some(configured_effort.clone()))
        .flatten();
    RealtimeHandoffRoutingDecision {
        selected_effort: selected_effort.clone(),
        routing: RealtimeHandoffRouting {
            classifier: RealtimeHandoffClassifier {
                kind: RealtimeHandoffClassifierKind::Model,
                model: Some(classifier_model),
                reasoning_effort: classifier_reasoning_effort,
                fallback: None,
            },
            classification,
            selected_effort,
        },
    }
}

fn text_fallback_decision(
    input: &str,
    configured_effort: &ReasoningEffort,
    classifier_model: String,
    classifier_reasoning_effort: Option<ReasoningEffort>,
    failure: ClassifierFailure,
) -> RealtimeHandoffRoutingDecision {
    let fallback = match failure {
        ClassifierFailure::InputTooLong => RealtimeHandoffClassifierFallback::InputTooLong,
        ClassifierFailure::InvalidOutput => RealtimeHandoffClassifierFallback::InvalidOutput,
        ClassifierFailure::RequestFailed => RealtimeHandoffClassifierFallback::RequestFailed,
        ClassifierFailure::TimedOut => RealtimeHandoffClassifierFallback::TimedOut,
    };
    text_decision(
        input,
        Some(configured_effort),
        Some(RealtimeHandoffClassifier {
            kind: RealtimeHandoffClassifierKind::Text,
            model: Some(classifier_model),
            reasoning_effort: classifier_reasoning_effort,
            fallback: Some(fallback),
        }),
    )
}

async fn classify_with_model(
    sess: &Session,
    input: &str,
    handoff_id: &str,
    classifier_model: &str,
    classifier_reasoning_effort: Option<&ReasoningEffort>,
) -> Result<RealtimeHandoffClassification, ClassifierFailure> {
    let config = sess.get_config().await;
    let model_info = sess
        .services
        .models_manager
        .get_model_info(classifier_model, &config.to_models_manager_config())
        .await;
    let bounded_input = take_bytes_at_char_boundary(input, CLASSIFIER_INPUT_MAX_BYTES);
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!(
                    "Classify this request:\n<user_request>\n{bounded_input}\n</user_request>"
                ),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        tools: Vec::new(),
        parallel_tool_calls: false,
        base_instructions: BaseInstructions {
            text: CLASSIFIER_INSTRUCTIONS.to_string(),
        },
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "classification": {
                    "type": "string",
                    "enum": ["read_only", "substantive"]
                }
            },
            "required": ["classification"],
            "additionalProperties": false
        })),
        output_schema_strict: true,
    };
    let mut metadata = CodexResponsesMetadata::new(
        sess.installation_id.clone(),
        sess.session_id().to_string(),
        sess.thread_id().to_string(),
        sess.current_window_id().await,
    );
    metadata.turn_id = Some(format!("realtime-classifier-{handoff_id}"));
    metadata.request_kind = Some(CodexResponsesRequestKind::Turn);
    let telemetry = SessionTelemetry::with_model(
        sess.services.session_telemetry.clone(),
        model_info.slug.as_str(),
        model_info.slug.as_str(),
    );
    let mut client_session = sess.services.model_client.new_session();
    let mut stream = client_session
        .stream(
            &prompt,
            &model_info,
            &telemetry,
            classifier_reasoning_effort.cloned(),
            ReasoningSummary::None,
            None,
            &metadata,
            &InferenceTraceContext::disabled(),
        )
        .await
        .map_err(|_| ClassifierFailure::RequestFailed)?;
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event.map_err(|_| ClassifierFailure::RequestFailed)? {
            crate::ResponseEvent::OutputTextDelta(delta) => {
                if output.len().saturating_add(delta.len()) > CLASSIFIER_OUTPUT_MAX_BYTES {
                    return Err(ClassifierFailure::InvalidOutput);
                }
                output.push_str(&delta);
            }
            crate::ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. })
                if output.is_empty() =>
            {
                for item in content {
                    if let ContentItem::OutputText { text } = item {
                        if output.len().saturating_add(text.len()) > CLASSIFIER_OUTPUT_MAX_BYTES {
                            return Err(ClassifierFailure::InvalidOutput);
                        }
                        output.push_str(&text);
                    }
                }
            }
            crate::ResponseEvent::Completed { .. } => break,
            _ => {}
        }
    }
    let parsed = serde_json::from_str::<ClassifierOutput>(output.trim())
        .map_err(|_| ClassifierFailure::InvalidOutput)?;
    debug!(
        handoff_id,
        classifier_model,
        classification = ?parsed.classification,
        "classified realtime handoff"
    );
    Ok(parsed.classification)
}

#[cfg(test)]
#[path = "realtime_classifier_tests.rs"]
mod tests;
