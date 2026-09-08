use std::collections::HashMap;
use std::collections::HashSet;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

use crate::SessionId;
use crate::ThreadId;

use super::TokenUsage;

/// Model/provider context retained with one exact upstream response usage record.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, Hash, JsonSchema, TS)]
pub struct TokenUsageAttribution {
    /// Model that produced the response, when known.
    #[serde(default)]
    pub model: Option<String>,
    /// Provider that produced the response, when known.
    #[serde(default)]
    pub model_provider: Option<String>,
    /// Effective service tier used for this response, when known.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Pricing context-length bucket captured when the response was recorded.
    #[serde(default)]
    pub context_length: Option<String>,
}

/// Best-effort Responses API usage observed for one completed response.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsageRecord {
    pub thread_id: ThreadId,
    /// Parent thread that owns this source response when it is forwarded from a child.
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    pub turn_id: String,
    pub session_id: SessionId,
    pub root_turn_id: String,
    pub response_id: String,
    pub usage: TokenUsage,
    pub turn_token_usage: TokenUsage,
    pub thread_token_usage: TokenUsage,
    /// Source model/provider/tier and context bucket.
    #[serde(default)]
    pub attribution: TokenUsageAttribution,
    /// Unix timestamp in milliseconds when the response completed.
    #[serde(default)]
    pub completed_at_ms: Option<i64>,
}

/// Exact identity of one response included in a usage projection.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsageResponseIdentity {
    pub thread_id: ThreadId,
    pub response_id: String,
}

/// Aggregate usage for one attributed source thread/model/provider/context bucket.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsageProjectionSource {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub attribution: TokenUsageAttribution,
    pub usage: TokenUsage,
    /// Response ids represented by this source aggregate.
    #[serde(default)]
    pub response_ids: Vec<String>,
}

/// Usage projection for one thread in a recursively reconstructed tree.
///
/// Empty `sources` and `response_ids` are intentional: a thread can have durable descendants even
/// when it emitted no usage-bearing response itself. Keeping that edge in the projection lets
/// consumers display and traverse the complete tree without inferring parentage from usage.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsageProjectionThread {
    pub thread_id: ThreadId,
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    #[serde(default)]
    pub forked_from_id: Option<ThreadId>,
    #[serde(default)]
    pub sources: Vec<TokenUsageProjectionSource>,
    #[serde(default)]
    pub response_ids: Vec<TokenUsageResponseIdentity>,
}

/// Billing-oriented usage projection reconstructed from exact response records.
///
/// This projection is separate from `TokenUsageInfo`, whose totals describe the active model
/// context window. Consumers can merge a replayed projection with live exact completions by
/// checking per-thread response identities without relying on cumulative counters as ordering
/// watermarks.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct TokenUsageProjection {
    pub total_usage: TokenUsage,
    #[serde(default)]
    pub threads: Vec<TokenUsageProjectionThread>,
}

impl TokenUsageProjection {
    /// Reconstruct a deduplicated billing projection from complete response records and durable
    /// thread metadata. The metadata list may contain threads with no records; those threads stay
    /// in the result so recursive parentage does not depend on whether a model reported usage.
    pub fn from_threads_and_records(
        threads: impl IntoIterator<Item = TokenUsageProjectionThread>,
        records: impl IntoIterator<Item = TokenUsageRecord>,
    ) -> Self {
        let mut projection = Self::default();
        let mut thread_indices = HashMap::<ThreadId, usize>::new();
        for mut thread in threads {
            if let Some(index) = thread_indices.get(&thread.thread_id).copied() {
                let existing = &mut projection.threads[index];
                if existing.parent_thread_id.is_none() {
                    existing.parent_thread_id = thread.parent_thread_id.take();
                }
                if existing.forked_from_id.is_none() {
                    existing.forked_from_id = thread.forked_from_id.take();
                }
                continue;
            }
            let index = projection.threads.len();
            thread_indices.insert(thread.thread_id, index);
            projection.threads.push(thread);
        }

        let mut seen_responses = HashSet::<(ThreadId, String)>::new();
        let mut source_indices = HashMap::<(ThreadId, TokenUsageAttribution), usize>::new();
        for record in records {
            let identity = (record.thread_id, record.response_id.clone());
            if !seen_responses.insert(identity) {
                continue;
            }

            let thread_index = *thread_indices.entry(record.thread_id).or_insert_with(|| {
                let index = projection.threads.len();
                projection.threads.push(TokenUsageProjectionThread {
                    thread_id: record.thread_id,
                    parent_thread_id: record.parent_thread_id,
                    ..Default::default()
                });
                index
            });
            let source_key = (record.thread_id, record.attribution.clone());
            let source_index = *source_indices.entry(source_key).or_insert_with(|| {
                let index = projection.threads[thread_index].sources.len();
                projection.threads[thread_index]
                    .sources
                    .push(TokenUsageProjectionSource {
                        thread_id: record.thread_id,
                        attribution: record.attribution.clone(),
                        usage: TokenUsage::default(),
                        response_ids: Vec::new(),
                    });
                index
            });
            let source = &mut projection.threads[thread_index].sources[source_index];
            source.usage.add_assign(&record.usage);
            source.response_ids.push(record.response_id.clone());
            projection.threads[thread_index]
                .response_ids
                .push(TokenUsageResponseIdentity {
                    thread_id: record.thread_id,
                    response_id: record.response_id,
                });
            projection.total_usage.add_assign(&record.usage);
        }

        projection
            .threads
            .sort_by_key(|thread| thread.thread_id.to_string());
        for thread in &mut projection.threads {
            thread
                .response_ids
                .sort_by(|left, right| left.response_id.cmp(&right.response_id));
            thread.sources.sort_by(|left, right| {
                left.attribution
                    .model_provider
                    .cmp(&right.attribution.model_provider)
                    .then_with(|| left.attribution.model.cmp(&right.attribution.model))
                    .then_with(|| {
                        left.attribution
                            .service_tier
                            .cmp(&right.attribution.service_tier)
                    })
                    .then_with(|| {
                        left.attribution
                            .context_length
                            .cmp(&right.attribution.context_length)
                    })
            });
            for source in &mut thread.sources {
                source.response_ids.sort();
            }
        }
        projection
    }
}

#[cfg(test)]
#[path = "token_usage_tests.rs"]
mod tests;
