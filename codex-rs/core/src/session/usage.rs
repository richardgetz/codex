//! Exact response usage recording and replay reconstruction.
//!
//! Context-window snapshots and billing records have different lifetimes. This module keeps the
//! record ledger and one-shot forwarding hooks together so the central session loop only owns
//! orchestration.

use super::session::Session;
use super::step_context::StepContext;
use super::turn_context::TurnContext;
use crate::turn_timing::now_unix_timestamp_ms;
use codex_history::RolloutItem;
use codex_protocol::ResponseUsageMetadata;
use codex_protocol::ThreadId;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RawResponseCompletedEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageAttribution;
use codex_protocol::protocol::TokenUsageRecord;

impl Session {
    /// Returns exact response usage records owned by this thread. Records copied into a fork are
    /// filtered during reconstruction, so callers never mistake inherited history for child spend.
    pub(crate) async fn token_usage_records(&self) -> Vec<TokenUsageRecord> {
        self.state.lock().await.token_usage_records.clone()
    }

    /// Extracts exact response records from replay history, including checkpoint copies. A
    /// caller combining multiple physical rollouts must still deduplicate response identities.
    pub(crate) fn token_usage_records_from_rollout(
        rollout_items: &[RolloutItem],
        thread_id: ThreadId,
    ) -> Vec<TokenUsageRecord> {
        rollout_items
            .iter()
            .filter_map(|item| match item {
                RolloutItem::TokenUsageRecord(record) if record.thread_id == thread_id => {
                    Some(record.clone())
                }
                RolloutItem::Compacted(compacted) => compacted
                    .latest_token_usage_record
                    .as_ref()
                    .filter(|record| record.thread_id == thread_id)
                    .cloned(),
                _ => None,
            })
            .collect()
    }

    pub(crate) async fn record_observed_response_completed(
        &self,
        turn_context: &TurnContext,
        response_id: &str,
        usage: Option<&TokenUsage>,
        usage_metadata: Option<&ResponseUsageMetadata>,
    ) {
        self.record_observed_response_completed_with_attribution(
            turn_context,
            response_id,
            usage,
            usage_metadata,
            /*effective_service_tier*/ None,
        )
        .await;
    }

    pub(crate) async fn record_observed_response_completed_with_attribution(
        &self,
        turn_context: &TurnContext,
        response_id: &str,
        usage: Option<&TokenUsage>,
        usage_metadata: Option<&ResponseUsageMetadata>,
        effective_service_tier: Option<&str>,
    ) {
        self.record_observed_response_completed_with_details(
            turn_context,
            response_id,
            usage,
            usage_metadata,
            turn_context.model_info().slug.as_str(),
            turn_context.config.model_provider_id.as_str(),
            effective_service_tier,
            turn_context.config.service_tier.as_deref(),
        )
        .await;
    }

    /// Records a completion using the immutable model settings captured for the sampling step.
    ///
    /// A turn can refresh its model between sampling requests. The turn context intentionally
    /// retains its initial model for compatibility, so response billing must use the step
    /// settings that actually built this request.
    pub(crate) async fn record_observed_response_completed_for_step(
        &self,
        step_context: &StepContext,
        response_id: &str,
        usage: Option<&TokenUsage>,
        usage_metadata: Option<&ResponseUsageMetadata>,
        effective_service_tier: Option<&str>,
    ) {
        let turn_context = &step_context.turn;
        self.record_observed_response_completed_with_details(
            turn_context,
            response_id,
            usage,
            usage_metadata,
            step_context.settings.model_info.slug.as_str(),
            turn_context.config.model_provider_id.as_str(),
            effective_service_tier,
            step_context.settings.service_tier.as_deref(),
        )
        .await;
    }

    async fn record_observed_response_completed_with_details(
        &self,
        turn_context: &TurnContext,
        response_id: &str,
        usage: Option<&TokenUsage>,
        usage_metadata: Option<&ResponseUsageMetadata>,
        model: &str,
        model_provider: &str,
        effective_service_tier: Option<&str>,
        fallback_service_tier: Option<&str>,
    ) {
        let model = Some(model.to_string());
        let model_provider = Some(model_provider.to_string());
        let service_tier = effective_service_tier
            .or(fallback_service_tier)
            .filter(|tier| !tier.eq_ignore_ascii_case(SERVICE_TIER_DEFAULT_REQUEST_VALUE))
            .map(str::to_string);
        let completed_at_ms = Some(now_unix_timestamp_ms());
        let usage_record = if let Some(usage) = usage {
            let attribution = TokenUsageAttribution {
                model: model.clone(),
                model_provider: model_provider.clone(),
                service_tier: service_tier.clone(),
                context_length: Some(usage.context_length().to_string()),
            };
            Some(
                self.state.lock().await.record_token_usage_with_attribution(
                    self.thread_id,
                    &turn_context.sub_id,
                    self.session_id(),
                    turn_context
                        .turn_metadata_state
                        .root_turn_id()
                        .unwrap_or_else(|| turn_context.sub_id.clone()),
                    response_id.to_string(),
                    usage,
                    turn_context.parent_thread_id,
                    attribution,
                    completed_at_ms,
                ),
            )
        } else {
            None
        };
        self.send_event(
            turn_context,
            EventMsg::RawResponseCompleted(RawResponseCompletedEvent {
                response_id: response_id.to_string(),
                token_usage: usage.cloned(),
                usage_metadata: usage_metadata.cloned(),
                thread_id: Some(self.thread_id),
                parent_thread_id: turn_context.parent_thread_id,
                completed_at_ms,
                usage_record: usage_record.clone(),
            }),
        )
        .await;
        if let Some(record) = usage_record {
            self.persist_rollout_items(&[RolloutItem::TokenUsageRecord(record)])
                .await;
        }
    }

    /// Retain an exact child response in this thread's rollout when a one-shot delegate forwards
    /// its completion through the parent. The record keeps the child source identity so replay can
    /// include ephemeral review usage without adding it to the parent's context totals.
    pub(crate) async fn persist_forwarded_response_usage(&self, record: TokenUsageRecord) {
        self.persist_rollout_items(&[RolloutItem::TokenUsageRecord(record)])
            .await;
    }
}
