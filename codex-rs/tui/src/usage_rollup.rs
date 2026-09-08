//! Exact usage aggregation for a thread and its subagent tree.
//!
//! Context-window usage and billed usage have different meanings. The former is supplied by the
//! current thread's `TokenUsageInfo` and remains owned by `ChatWidget`; this module stores only
//! response-record data used by `/status` and `/spend`. A complete projection contains aggregate
//! usage and the identities represented by that aggregate. Live records are kept separately until
//! a later projection proves that the same identity is already included.

use crate::daily_spend::DailySpendRecord;
use crate::daily_spend::DailySpendTracker;
use crate::token_usage::TokenUsage;
use crate::token_usage::TokenUsageInfo;
use codex_config::types::TuiStatusTokenUsage;
use codex_protocol::ThreadId;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

#[path = "usage_rollup_projection.rs"]
mod usage_rollup_projection;

/// One aggregate source in a complete persisted usage projection.
///
/// `response_ids` is required for exact merging. Aggregate counters alone cannot tell whether a
/// late projection includes a live response, especially when two responses have equal usage. The
/// server may omit model/provider/tier for legacy records; those records remain visible in token
/// totals but cannot be priced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageRollupSource {
    pub(crate) thread_id: ThreadId,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) forked_from_id: Option<ThreadId>,
    pub(crate) model_provider_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) service_tier: Option<String>,
    pub(crate) context_length: Option<String>,
    pub(crate) usage: TokenUsage,
    pub(crate) usage_by_service_tier: BTreeMap<String, TokenUsage>,
    pub(crate) usage_by_service_tier_and_context_length:
        BTreeMap<String, BTreeMap<String, TokenUsage>>,
    /// Response ids represented by `usage` for this source/thread.
    pub(crate) response_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UsageRollupSnapshot {
    pub(crate) total_usage: TokenUsage,
    pub(crate) sources: Vec<UsageRollupSource>,
    /// True when the backend supplied a complete projection for this root, including a known
    /// zero-usage projection. This keeps an empty fork from falling back to inherited context
    /// counters in the status card.
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResponseKey {
    thread_id: ThreadId,
    response_id: String,
}

impl Ord for ResponseKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.thread_id
            .to_string()
            .cmp(&other.thread_id.to_string())
            .then_with(|| self.response_id.cmp(&other.response_id))
    }
}

impl PartialOrd for ResponseKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SourceKey {
    thread_id: ThreadId,
    model_provider_id: Option<String>,
    model: Option<String>,
    service_tier: Option<String>,
    context_length: Option<String>,
}

impl Ord for SourceKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.thread_id
            .to_string()
            .cmp(&other.thread_id.to_string())
            .then_with(|| self.model_provider_id.cmp(&other.model_provider_id))
            .then_with(|| self.model.cmp(&other.model))
            .then_with(|| self.service_tier.cmp(&other.service_tier))
            .then_with(|| self.context_length.cmp(&other.context_length))
    }
}

impl PartialOrd for SourceKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
struct LiveUsageRecord {
    source_key: SourceKey,
    usage: TokenUsage,
}

#[derive(Debug, Default)]
struct UsageNode {
    parent_thread_id: Option<ThreadId>,
    forked_from_id: Option<ThreadId>,
    /// Complete persisted aggregate sources owned by this thread. A fork therefore does not
    /// inherit its source thread's billed usage.
    baseline: BTreeMap<SourceKey, UsageRollupSource>,
}

/// Shared, process-local view of exact usage for one session tree.
pub(crate) struct UsageRollup {
    nodes: HashMap<ThreadId, UsageNode>,
    baseline_source_by_response: HashMap<ResponseKey, SourceKey>,
    complete_projection_threads: HashSet<ThreadId>,
    live_records: BTreeMap<ResponseKey, LiveUsageRecord>,
    daily_spend: DailySpendTracker,
}

/// App-lifetime ownership for exact usage. Chat widgets are replaced when the selected thread
/// changes, so the rollup must live behind a shared handle owned by the application.
#[derive(Clone)]
pub(crate) struct SharedUsageRollup(Arc<Mutex<UsageRollup>>);

impl SharedUsageRollup {
    pub(crate) fn new(codex_home: &Path) -> Self {
        Self(Arc::new(Mutex::new(UsageRollup::new(codex_home))))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, UsageRollup> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl UsageRollup {
    pub(crate) fn new(codex_home: &Path) -> Self {
        Self {
            nodes: HashMap::new(),
            baseline_source_by_response: HashMap::new(),
            complete_projection_threads: HashSet::new(),
            live_records: BTreeMap::new(),
            daily_spend: DailySpendTracker::new_exact_records_only(codex_home),
        }
    }

    /// Registers a thread edge used for recursive traversal.
    pub(crate) fn observe_thread(
        &mut self,
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
        forked_from_id: Option<ThreadId>,
    ) {
        let node = self.nodes.entry(thread_id).or_default();
        if parent_thread_id.is_some() {
            node.parent_thread_id = parent_thread_id;
        }
        if forked_from_id.is_some() {
            node.forked_from_id = forked_from_id;
        }
    }

    /// Merges a complete persisted projection.
    ///
    /// The merge is identity-aware. A repeated or stale projection cannot erase live records, and
    /// a projection that contains only a subset of a previous baseline cannot decrease totals.
    /// Source aggregates are treated as one authoritative snapshot once the identity set proves
    /// that it includes the previous baseline. No parent snapshot or fork subtraction is used.
    pub(crate) fn observe_complete_projection(
        &mut self,
        root_thread_id: ThreadId,
        sources: impl IntoIterator<Item = UsageRollupSource>,
    ) {
        self.nodes.entry(root_thread_id).or_default();
        self.complete_projection_threads.insert(root_thread_id);
        for source in sources {
            self.observe_thread(
                source.thread_id,
                source.parent_thread_id,
                source.forked_from_id,
            );
            self.complete_projection_threads.insert(source.thread_id);
            let source_key = source_key(&source);
            let incoming_ids = response_keys(source.thread_id, &source.response_ids);
            if incoming_ids.iter().any(|response_key| {
                self.baseline_source_by_response
                    .get(response_key)
                    .is_some_and(|owner| owner != &source_key)
            }) {
                // A response identity must belong to one immutable attribution source. If an
                // inconsistent projection moves an already-accounted response to another source,
                // retaining the first source avoids double billing and preserves exact totals.
                continue;
            }
            let previous_ids = self
                .nodes
                .get(&source.thread_id)
                .and_then(|node| node.baseline.get(&source_key))
                .map(|previous| {
                    (
                        response_keys(previous.thread_id, &previous.response_ids),
                        !previous.usage.is_zero(),
                    )
                });
            let Some(previous_ids) = previous_ids else {
                self.record_baseline_identities(&incoming_ids, &source_key);
                self.nodes
                    .entry(source.thread_id)
                    .or_default()
                    .baseline
                    .insert(source_key, source);
                continue;
            };
            // A complete source without identities cannot establish ordering, so it cannot
            // replace an identity-bearing baseline. An identity-bearing projection replaces an
            // older baseline only when it proves that all prior responses are included. This
            // treats the counters as one authoritative snapshot and never adds a cumulative
            // snapshot delta a second time.
            let (previous_ids, previous_has_usage) = previous_ids;
            if incoming_ids.is_empty()
                || (previous_ids.is_empty() && previous_has_usage)
                || !previous_ids.is_subset(&incoming_ids)
            {
                continue;
            }
            self.nodes
                .entry(source.thread_id)
                .or_default()
                .baseline
                .insert(source_key.clone(), source);
            self.record_baseline_identities(&incoming_ids, &source_key);
            for response_key in &incoming_ids {
                self.live_records.remove(response_key);
            }
        }
    }

    /// Marks a previously projected root unavailable while retaining all exact/live usage.
    pub(crate) fn mark_projection_unavailable(&mut self, root_thread_id: ThreadId) {
        self.complete_projection_threads.remove(&root_thread_id);
    }

    fn record_baseline_identities(
        &mut self,
        response_ids: &HashSet<ResponseKey>,
        source_key: &SourceKey,
    ) {
        self.baseline_source_by_response.extend(
            response_ids
                .iter()
                .cloned()
                .map(|response_key| (response_key, source_key.clone())),
        );
    }

    fn observe_response_with_context_length(
        &mut self,
        thread_id: ThreadId,
        parent_thread_id: Option<ThreadId>,
        forked_from_id: Option<ThreadId>,
        response_id: &str,
        usage: Option<&TokenUsage>,
        model_provider_id: Option<&str>,
        model: Option<&str>,
        service_tier: Option<&str>,
        context_length: Option<&str>,
        recorded_at: Option<i64>,
        config: &TuiStatusTokenUsage,
    ) {
        self.observe_thread(thread_id, parent_thread_id, forked_from_id);
        let Some(usage) = usage else {
            return;
        };
        let response_key = ResponseKey {
            thread_id,
            response_id: response_id.to_string(),
        };
        if self.baseline_source_by_response.contains_key(&response_key)
            || self.live_records.contains_key(&response_key)
        {
            self.record_daily_response(
                &response_key,
                usage,
                model_provider_id,
                model,
                service_tier,
                context_length,
                recorded_at,
                config,
            );
            return;
        }
        let source_key = SourceKey {
            thread_id,
            model_provider_id: model_provider_id.map(str::to_string),
            model: model.map(str::to_string),
            service_tier: service_tier.map(str::to_string),
            context_length: context_length.map(str::to_string),
        };
        self.live_records.insert(
            response_key.clone(),
            LiveUsageRecord {
                source_key,
                usage: usage.clone(),
            },
        );
        self.record_daily_response(
            &response_key,
            usage,
            model_provider_id,
            model,
            service_tier,
            context_length,
            recorded_at,
            config,
        );
    }

    fn record_daily_response(
        &mut self,
        response_key: &ResponseKey,
        usage: &TokenUsage,
        model_provider_id: Option<&str>,
        model: Option<&str>,
        service_tier: Option<&str>,
        context_length: Option<&str>,
        recorded_at: Option<i64>,
        config: &TuiStatusTokenUsage,
    ) {
        let response_key = response_key_string(response_key);
        if let Err(error) = self.daily_spend.observe_record(
            DailySpendRecord {
                response_key: &response_key,
                usage,
                model_provider_id,
                model,
                service_tier,
                context_length,
                recorded_at,
            },
            config,
        ) {
            tracing::warn!(%error, "failed to record daily spend for response");
        }
    }

    /// Returns recursive own-plus-descendant usage for a root thread.
    pub(crate) fn snapshot_for(&self, root_thread_id: ThreadId) -> UsageRollupSnapshot {
        let mut children = HashMap::<ThreadId, Vec<ThreadId>>::new();
        for (thread_id, node) in &self.nodes {
            // An ordinary fork starts an independent billing tree. Spawned Workers and reviews
            // also carry a fork source because they inherit context, but their explicit parent
            // makes their own response records part of the parent's billed subtree. The backend
            // projection excludes copied ancestor records from those children.
            if node.forked_from_id.is_some() && node.parent_thread_id.is_none() {
                continue;
            }
            if let Some(parent_thread_id) = node.parent_thread_id {
                children
                    .entry(parent_thread_id)
                    .or_default()
                    .push(*thread_id);
            }
        }
        let mut sources = BTreeMap::<SourceKey, UsageRollupSource>::new();
        let mut pending = vec![root_thread_id];
        let mut visited = HashSet::new();
        while let Some(thread_id) = pending.pop() {
            if !visited.insert(thread_id) {
                continue;
            }
            if let Some(node) = self.nodes.get(&thread_id) {
                for (key, source) in &node.baseline {
                    merge_source(&mut sources, key.clone(), source.clone());
                }
            }
            for (response_key, live) in &self.live_records {
                if response_key.thread_id == thread_id
                    && !self.baseline_source_by_response.contains_key(response_key)
                {
                    merge_source(
                        &mut sources,
                        live.source_key.clone(),
                        live_source(response_key, live, self.nodes.get(&thread_id)),
                    );
                }
            }
            if let Some(children) = children.get(&thread_id) {
                pending.extend(children.iter().copied());
            }
        }
        // Empty source nodes still matter for preserving parent edges, but should not replace a
        // direct token snapshot in the status card with an all-zero recursive result.
        let sources = sources
            .into_values()
            .filter(|source| !source.usage.is_zero())
            .collect::<Vec<_>>();
        let total_usage = sources
            .iter()
            .fold(TokenUsage::default(), |mut total, source| {
                add_usage(&mut total, &source.usage);
                total
            });
        UsageRollupSnapshot {
            total_usage,
            sources,
            complete: self.complete_projection_threads.contains(&root_thread_id),
        }
    }

    pub(crate) fn render_report(
        &self,
        args: &str,
    ) -> anyhow::Result<Vec<ratatui::text::Line<'static>>> {
        self.daily_spend.render_report(args)
    }

    pub(crate) fn observe_legacy_snapshot(
        &mut self,
        info: &TokenUsageInfo,
        replay: bool,
        config: &TuiStatusTokenUsage,
        model_provider_id: &str,
        current_model: &str,
    ) -> anyhow::Result<()> {
        self.daily_spend
            .observe(info, replay, config, model_provider_id, current_model)
    }
}

fn source_key(source: &UsageRollupSource) -> SourceKey {
    SourceKey {
        thread_id: source.thread_id,
        model_provider_id: source.model_provider_id.clone(),
        model: source.model.clone(),
        service_tier: source.service_tier.clone(),
        context_length: source.context_length.clone(),
    }
}

fn response_keys(thread_id: ThreadId, response_ids: &[String]) -> HashSet<ResponseKey> {
    response_ids
        .iter()
        .map(|response_id| ResponseKey {
            thread_id,
            response_id: response_id.clone(),
        })
        .collect()
}

fn response_key_string(response_key: &ResponseKey) -> String {
    format!("{}:{}", response_key.thread_id, response_key.response_id)
}

fn live_source(
    response_key: &ResponseKey,
    live: &LiveUsageRecord,
    node: Option<&UsageNode>,
) -> UsageRollupSource {
    usage_rollup_projection::source_from_parts(
        response_key.thread_id,
        node.and_then(|node| node.parent_thread_id),
        node.and_then(|node| node.forked_from_id),
        live.source_key.model_provider_id.as_deref(),
        live.source_key.model.as_deref(),
        live.source_key.service_tier.as_deref(),
        live.source_key.context_length.as_deref(),
        live.usage.clone(),
        vec![response_key.response_id.clone()],
    )
}

fn merge_source(
    sources: &mut BTreeMap<SourceKey, UsageRollupSource>,
    key: SourceKey,
    incoming: UsageRollupSource,
) {
    let Some(existing) = sources.get_mut(&key) else {
        sources.insert(key, incoming);
        return;
    };
    add_usage(&mut existing.usage, &incoming.usage);
    merge_usage_maps(
        &mut existing.usage_by_service_tier,
        &incoming.usage_by_service_tier,
    );
    merge_nested_usage_maps(
        &mut existing.usage_by_service_tier_and_context_length,
        &incoming.usage_by_service_tier_and_context_length,
    );
    for response_id in incoming.response_ids {
        if !existing.response_ids.contains(&response_id) {
            existing.response_ids.push(response_id);
        }
    }
}

fn merge_usage_maps(
    target: &mut BTreeMap<String, TokenUsage>,
    incoming: &BTreeMap<String, TokenUsage>,
) {
    for (key, usage) in incoming {
        add_usage(target.entry(key.clone()).or_default(), usage);
    }
}

fn merge_nested_usage_maps(
    target: &mut BTreeMap<String, BTreeMap<String, TokenUsage>>,
    incoming: &BTreeMap<String, BTreeMap<String, TokenUsage>>,
) {
    for (tier, context_usages) in incoming {
        for (context_length, usage) in context_usages {
            add_usage(
                target
                    .entry(tier.clone())
                    .or_default()
                    .entry(context_length.clone())
                    .or_default(),
                usage,
            );
        }
    }
}

fn add_usage(total: &mut TokenUsage, usage: &TokenUsage) {
    total.input_tokens += usage.input_tokens;
    total.cached_input_tokens += usage.cached_input_tokens;
    total.cache_write_tokens += usage.cache_write_tokens;
    total.output_tokens += usage.output_tokens;
    total.reasoning_output_tokens += usage.reasoning_output_tokens;
    total.total_tokens += usage.total_tokens;
}

#[cfg(test)]
#[path = "usage_rollup_tests.rs"]
mod tests;
