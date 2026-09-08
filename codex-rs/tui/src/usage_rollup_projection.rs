//! App-server adapters for the exact usage rollup.

use super::UsageRollup;
use super::UsageRollupSource;
use crate::token_usage::TokenUsage;
use codex_app_server_protocol::RawResponseCompletedNotification;
use codex_app_server_protocol::ThreadStartedNotification;
use codex_app_server_protocol::ThreadTokenUsageProjection;
use codex_protocol::ThreadId;
use codex_protocol::protocol::TOKEN_USAGE_STANDARD_SERVICE_TIER;
use std::collections::BTreeMap;

impl UsageRollup {
    /// Registers a live thread edge before its first response arrives. This keeps an empty
    /// intermediate worker reachable when a grandchild later reports usage.
    pub(crate) fn observe_app_thread_started(
        &mut self,
        notification: &ThreadStartedNotification,
    ) -> anyhow::Result<()> {
        let thread = &notification.thread;
        let thread_id = ThreadId::from_string(&thread.id)?;
        let parent_thread_id = thread
            .parent_thread_id
            .as_deref()
            .map(ThreadId::from_string)
            .transpose()?;
        let forked_from_id = thread
            .forked_from_id
            .as_deref()
            .map(ThreadId::from_string)
            .transpose()?;
        self.observe_thread(thread_id, parent_thread_id, forked_from_id);
        Ok(())
    }

    /// Merges a complete app-server projection. Thread edges are registered before source
    /// conversion so empty intermediate workers remain connected to their descendants.
    pub(crate) fn observe_app_projection(
        &mut self,
        root_thread_id: &str,
        projection: &ThreadTokenUsageProjection,
    ) -> anyhow::Result<()> {
        let root_thread_id = ThreadId::from_string(root_thread_id)?;
        let mut parsed_threads = Vec::with_capacity(projection.threads.len());
        let mut sources = Vec::new();
        for thread in &projection.threads {
            let thread_id = ThreadId::from_string(&thread.thread_id)?;
            let parent_thread_id = thread
                .parent_thread_id
                .as_deref()
                .map(ThreadId::from_string)
                .transpose()?;
            let forked_from_id = thread
                .forked_from_id
                .as_deref()
                .map(ThreadId::from_string)
                .transpose()?;
            for source in &thread.sources {
                sources.push(app_projection_source(
                    parent_thread_id,
                    forked_from_id,
                    source,
                )?);
            }
            parsed_threads.push((thread_id, parent_thread_id, forked_from_id));
        }

        // Parse the complete payload before mutating the live tree. A malformed later source
        // must not leave an earlier edge or completeness marker behind.
        self.daily_spend.mark_exact_records_observed();
        self.nodes.entry(root_thread_id).or_default();
        for (thread_id, parent_thread_id, forked_from_id) in parsed_threads {
            self.observe_thread(thread_id, parent_thread_id, forked_from_id);
            self.complete_projection_threads.insert(thread_id);
        }
        self.observe_complete_projection(root_thread_id, sources);
        Ok(())
    }

    /// Records that a complete projection could not be reconstructed for a root. Existing exact
    /// records remain usable, but status must wait for a later complete projection.
    pub(crate) fn observe_app_projection_unavailable(
        &mut self,
        root_thread_id: &str,
    ) -> anyhow::Result<()> {
        let root_thread_id = ThreadId::from_string(root_thread_id)?;
        self.mark_projection_unavailable(root_thread_id);
        self.nodes.entry(root_thread_id).or_default();
        Ok(())
    }

    /// Consumes an exact app-server completion, including forwarded review/worker responses.
    /// `source_thread_id` identifies the billed source when the event was delivered through its
    /// parent thread; the notification's thread id remains only the forwarding destination.
    pub(crate) fn observe_app_response(
        &mut self,
        notification: &RawResponseCompletedNotification,
        config: &codex_config::types::TuiStatusTokenUsage,
    ) -> anyhow::Result<()> {
        let thread_id = notification
            .source_thread_id
            .as_deref()
            .unwrap_or(&notification.thread_id);
        let thread_id = ThreadId::from_string(thread_id)?;
        let parent_thread_id = notification
            .parent_thread_id
            .as_deref()
            .map(ThreadId::from_string)
            .transpose()?;
        let attribution = notification.attribution.as_ref();
        let model = attribution.and_then(|attribution| attribution.model.as_deref());
        let model_provider_id =
            attribution.and_then(|attribution| attribution.model_provider.as_deref());
        let service_tier = attribution.and_then(|attribution| attribution.service_tier.as_deref());
        let context_length =
            attribution.and_then(|attribution| attribution.context_length.as_deref());
        let usage = notification.usage.as_ref().map(app_usage);
        self.observe_response_with_context_length(
            thread_id,
            parent_thread_id,
            None,
            &notification.response_id,
            usage.as_ref(),
            model_provider_id,
            model,
            service_tier,
            context_length,
            notification.completed_at,
            config,
        );
        Ok(())
    }
}

pub(super) fn source_from_parts(
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    forked_from_id: Option<ThreadId>,
    model_provider_id: Option<&str>,
    model: Option<&str>,
    service_tier: Option<&str>,
    context_length: Option<&str>,
    usage: TokenUsage,
    response_ids: Vec<String>,
) -> UsageRollupSource {
    let (usage_by_service_tier, usage_by_service_tier_and_context_length) =
        usage_maps(&usage, service_tier, context_length);
    UsageRollupSource {
        thread_id,
        parent_thread_id,
        forked_from_id,
        model_provider_id: model_provider_id.map(str::to_string),
        model: model.map(str::to_string),
        service_tier: service_tier.map(str::to_string),
        context_length: context_length.map(str::to_string),
        usage,
        usage_by_service_tier,
        usage_by_service_tier_and_context_length,
        response_ids,
    }
}

fn app_usage(value: &codex_app_server_protocol::TokenUsageBreakdown) -> TokenUsage {
    TokenUsage {
        total_tokens: value.total_tokens,
        input_tokens: value.input_tokens,
        cached_input_tokens: value.cached_input_tokens,
        cache_write_tokens: value.cache_write_input_tokens,
        output_tokens: value.output_tokens,
        reasoning_output_tokens: value.reasoning_output_tokens,
    }
}

fn app_projection_source(
    thread_parent: Option<ThreadId>,
    thread_fork: Option<ThreadId>,
    source: &codex_app_server_protocol::ThreadTokenUsageSource,
) -> anyhow::Result<UsageRollupSource> {
    let thread_id = ThreadId::from_string(&source.thread_id)?;
    Ok(source_from_parts(
        thread_id,
        thread_parent,
        thread_fork,
        source.attribution.model_provider.as_deref(),
        source.attribution.model.as_deref(),
        source.attribution.service_tier.as_deref(),
        source.attribution.context_length.as_deref(),
        app_usage(&source.usage),
        source.response_ids.clone(),
    ))
}

fn usage_maps(
    usage: &TokenUsage,
    service_tier: Option<&str>,
    context_length: Option<&str>,
) -> (
    BTreeMap<String, TokenUsage>,
    BTreeMap<String, BTreeMap<String, TokenUsage>>,
) {
    let mut usage_by_service_tier = BTreeMap::new();
    let mut usage_by_service_tier_and_context_length =
        BTreeMap::<String, BTreeMap<String, TokenUsage>>::new();
    // Providers often omit the standard tier. Preserve that attribution as `None`, while using
    // the canonical tier key for pricing buckets so each response's context length remains
    // visible to the cost calculator.
    let service_tier = service_tier.unwrap_or(TOKEN_USAGE_STANDARD_SERVICE_TIER);
    usage_by_service_tier.insert(service_tier.to_string(), usage.clone());
    if let Some(context_length) = context_length {
        usage_by_service_tier_and_context_length
            .entry(service_tier.to_string())
            .or_default()
            .insert(context_length.to_string(), usage.clone());
    }
    (
        usage_by_service_tier,
        usage_by_service_tier_and_context_length,
    )
}
