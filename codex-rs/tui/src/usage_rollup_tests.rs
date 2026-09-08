use super::*;
use crate::token_usage::TokenUsage;
use codex_app_server_protocol::ThreadTokenUsageAttribution;
use codex_app_server_protocol::ThreadTokenUsageProjection;
use codex_app_server_protocol::ThreadTokenUsageProjectionThread;
use codex_app_server_protocol::ThreadTokenUsageSource;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_protocol::protocol::TOKEN_USAGE_STANDARD_SERVICE_TIER;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens,
        total_tokens,
        ..TokenUsage::default()
    }
}

fn source(
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    forked_from_id: Option<ThreadId>,
    response_ids: &[&str],
    usage: TokenUsage,
    model_provider_id: Option<&str>,
    model: Option<&str>,
    service_tier: Option<&str>,
) -> UsageRollupSource {
    let context_length = (model_provider_id.is_some() && model.is_some()).then_some("short");
    let pricing_service_tier = service_tier.unwrap_or(TOKEN_USAGE_STANDARD_SERVICE_TIER);
    let mut source = UsageRollupSource {
        thread_id,
        parent_thread_id,
        forked_from_id,
        model_provider_id: model_provider_id.map(str::to_string),
        model: model.map(str::to_string),
        service_tier: service_tier.map(str::to_string),
        context_length: context_length.map(str::to_string),
        usage: usage.clone(),
        usage_by_service_tier: BTreeMap::from([(pricing_service_tier.to_string(), usage.clone())]),
        usage_by_service_tier_and_context_length: BTreeMap::new(),
        response_ids: response_ids.iter().map(ToString::to_string).collect(),
    };
    if let Some(context_length) = context_length {
        source
            .usage_by_service_tier_and_context_length
            .entry(pricing_service_tier.to_string())
            .or_default()
            .insert(context_length.to_string(), usage);
    }
    source
}

fn new_rollup() -> (UsageRollup, TempDir) {
    let temp_dir = TempDir::new().expect("temporary codex home");
    (UsageRollup::new(temp_dir.path()), temp_dir)
}

fn app_projection_source(
    thread_id: &str,
    response_id: &str,
    total_tokens: i64,
) -> ThreadTokenUsageSource {
    ThreadTokenUsageSource {
        thread_id: thread_id.to_string(),
        attribution: ThreadTokenUsageAttribution {
            model: Some("gpt-5.4".to_string()),
            model_provider: Some("openai".to_string()),
            service_tier: None,
            context_length: Some("short".to_string()),
        },
        response_ids: vec![response_id.to_string()],
        usage: TokenUsageBreakdown {
            total_tokens,
            input_tokens: total_tokens,
            ..TokenUsageBreakdown::default()
        },
    }
}

fn app_projection(
    thread_id: &str,
    sources: Vec<ThreadTokenUsageSource>,
) -> ThreadTokenUsageProjection {
    ThreadTokenUsageProjection {
        total: TokenUsageBreakdown::default(),
        threads: vec![ThreadTokenUsageProjectionThread {
            thread_id: thread_id.to_string(),
            parent_thread_id: None,
            forked_from_id: None,
            sources,
            response_ids: Vec::new(),
        }],
    }
}

#[test]
fn complete_projection_recursively_sums_children_and_grandchildren() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    let child = ThreadId::new();
    let grandchild = ThreadId::new();
    rollup.observe_complete_projection(
        root,
        [
            source(
                root,
                None,
                None,
                &["root-1"],
                usage(10),
                Some("astra"),
                Some("a"),
                Some("standard"),
            ),
            source(
                child,
                Some(root),
                None,
                &["child-1"],
                usage(20),
                Some("luna"),
                Some("l"),
                Some("flex"),
            ),
            source(
                grandchild,
                Some(child),
                None,
                &["grandchild-1"],
                usage(30),
                Some("luna"),
                Some("l"),
                Some("flex"),
            ),
        ],
    );

    let snapshot = rollup.snapshot_for(root);

    assert_eq!(snapshot.total_usage, usage(60));
    assert_eq!(snapshot.sources.len(), 3);
    assert_eq!(
        snapshot
            .sources
            .iter()
            .map(|source| source.thread_id)
            .collect::<Vec<_>>(),
        vec![root, child, grandchild]
    );
}

#[test]
fn repeated_and_late_projections_merge_by_response_identity() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    let config = TuiStatusTokenUsage::default();
    rollup.observe_complete_projection(
        root,
        [source(
            root,
            None,
            None,
            &["first"],
            usage(10),
            Some("astra"),
            Some("a"),
            None,
        )],
    );
    rollup.observe_response_with_context_length(
        root,
        None,
        None,
        "second",
        Some(&usage(5)),
        Some("astra"),
        Some("a"),
        None,
        /*context_length*/ None,
        /*recorded_at*/ None,
        &config,
    );

    // A stale replay omits the live response and must preserve it.
    rollup.observe_complete_projection(
        root,
        [source(
            root,
            None,
            None,
            &["first"],
            usage(10),
            Some("astra"),
            Some("a"),
            None,
        )],
    );
    assert_eq!(rollup.snapshot_for(root).total_usage, usage(15));

    // The complete projection now includes the live response. It must replace the live addition
    // exactly once, and repeated delivery must remain stable.
    rollup.observe_complete_projection(
        root,
        [source(
            root,
            None,
            None,
            &["first", "second"],
            usage(15),
            Some("astra"),
            Some("a"),
            None,
        )],
    );
    rollup.observe_complete_projection(
        root,
        [source(
            root,
            None,
            None,
            &["first", "second"],
            usage(15),
            Some("astra"),
            Some("a"),
            None,
        )],
    );
    assert_eq!(rollup.snapshot_for(root).total_usage, usage(15));
}

#[test]
fn identity_projection_cannot_erase_legacy_baseline_without_ids() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    let config = TuiStatusTokenUsage::default();
    rollup.observe_complete_projection(
        root,
        [source(
            root,
            None,
            None,
            &[],
            usage(100),
            Some("openai"),
            Some("gpt-5.4"),
            None,
        )],
    );
    rollup.observe_response_with_context_length(
        root,
        None,
        None,
        "new",
        Some(&usage(10)),
        Some("openai"),
        Some("gpt-5.4"),
        None,
        Some("short"),
        None,
        &config,
    );

    // The identity-bearing projection cannot prove that its aggregate includes the legacy
    // baseline, so it must leave the baseline and live response separately visible.
    rollup.observe_complete_projection(
        root,
        [source(
            root,
            None,
            None,
            &["new"],
            usage(10),
            Some("openai"),
            Some("gpt-5.4"),
            None,
        )],
    );

    assert_eq!(rollup.snapshot_for(root).total_usage, usage(110));
}

#[test]
fn fork_projection_does_not_inherit_source_thread_usage() {
    let (mut rollup, _temp_dir) = new_rollup();
    let parent = ThreadId::new();
    let fork = ThreadId::new();
    rollup.observe_complete_projection(
        parent,
        [source(
            parent,
            None,
            None,
            &["parent"],
            usage(100),
            Some("astra"),
            Some("a"),
            None,
        )],
    );
    rollup.observe_complete_projection(
        fork,
        [source(
            fork,
            None,
            Some(parent),
            &["fork"],
            usage(7),
            Some("luna"),
            Some("l"),
            Some("standard"),
        )],
    );

    assert_eq!(rollup.snapshot_for(fork).total_usage, usage(7));
    assert_eq!(rollup.snapshot_for(parent).total_usage, usage(100));
}

#[test]
fn spawned_fork_child_counts_own_usage_under_parent() {
    let (mut rollup, _temp_dir) = new_rollup();
    let parent = ThreadId::new();
    let child = ThreadId::new();
    rollup.observe_complete_projection(
        parent,
        [source(
            parent,
            None,
            None,
            &["parent"],
            usage(100),
            Some("astra"),
            Some("a"),
            None,
        )],
    );
    rollup.observe_complete_projection(
        parent,
        [source(
            child,
            Some(parent),
            Some(parent),
            &["child"],
            usage(7),
            Some("luna"),
            Some("l"),
            Some("standard"),
        )],
    );

    assert_eq!(rollup.snapshot_for(child).total_usage, usage(7));
    assert_eq!(rollup.snapshot_for(parent).total_usage, usage(107));
}

#[test]
fn mixed_provider_and_legacy_sources_remain_separate_and_unpriced() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    rollup.observe_complete_projection(
        root,
        [
            source(
                root,
                None,
                None,
                &["astra"],
                usage(11),
                Some("astra"),
                Some("a"),
                Some("standard"),
            ),
            source(
                root,
                None,
                None,
                &["luna"],
                usage(13),
                Some("luna"),
                Some("l"),
                Some("flex"),
            ),
            source(root, None, None, &["legacy"], usage(17), None, None, None),
        ],
    );

    let snapshot = rollup.snapshot_for(root);

    assert_eq!(snapshot.total_usage, usage(41));
    assert!(snapshot.sources.iter().any(|source| {
        source.model_provider_id.as_deref() == Some("astra") && source.model.as_deref() == Some("a")
    }));
    assert!(snapshot.sources.iter().any(|source| {
        source.model_provider_id.as_deref() == Some("luna") && source.model.as_deref() == Some("l")
    }));
    assert!(
        snapshot
            .sources
            .iter()
            .any(|source| source.model_provider_id.is_none() && source.model.is_none())
    );
}

#[test]
fn default_tier_preserves_per_response_context_bucket() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    let config = TuiStatusTokenUsage::default();
    let first_usage = usage(200_000);
    let second_usage = usage(200_000);

    rollup.observe_response_with_context_length(
        root,
        None,
        None,
        "first",
        Some(&first_usage),
        Some("openai"),
        Some("gpt-5.4"),
        None,
        Some("short"),
        None,
        &config,
    );
    rollup.observe_response_with_context_length(
        root,
        None,
        None,
        "second",
        Some(&second_usage),
        Some("openai"),
        Some("gpt-5.4"),
        None,
        Some("short"),
        None,
        &config,
    );

    let snapshot = rollup.snapshot_for(root);
    let source = &snapshot.sources[0];
    let total_usage = usage(400_000);
    assert_eq!(snapshot.total_usage, total_usage);
    assert_eq!(source.service_tier, None);
    assert_eq!(
        source.usage_by_service_tier,
        BTreeMap::from([(
            TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(),
            total_usage.clone()
        )])
    );
    assert_eq!(
        source.usage_by_service_tier_and_context_length,
        BTreeMap::from([(
            TOKEN_USAGE_STANDARD_SERVICE_TIER.to_string(),
            BTreeMap::from([("short".to_string(), total_usage)])
        )])
    );
}

#[test]
fn direct_snapshot_before_exact_response_does_not_bill_cumulative_history() {
    let (mut rollup, temp_dir) = new_rollup();
    let mut config = TuiStatusTokenUsage::default();
    config.enabled = true;
    let root = ThreadId::new();

    rollup
        .observe_legacy_snapshot(
            &TokenUsageInfo {
                total_token_usage: usage(100),
                ..TokenUsageInfo::default()
            },
            false,
            &config,
            "openai",
            "gpt-5.4",
        )
        .expect("direct snapshot should remain informational");
    rollup.observe_response_with_context_length(
        root,
        None,
        None,
        "new-response",
        Some(&usage(10)),
        Some("openai"),
        Some("gpt-5.4"),
        None,
        Some("short"),
        None,
        &config,
    );

    let history_path = temp_dir.path().join("usage").join("daily_spend.json");
    let history: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(history_path).expect("exact response should persist"),
    )
    .expect("daily history should deserialize");
    assert_eq!(
        history["days"].as_object().map(|days| days
            .values()
            .filter_map(|day| day["tokens"].as_i64())
            .sum::<i64>()),
        Some(10)
    );
    assert_eq!(
        history["response_dates"].as_object().map(|ids| ids.len()),
        Some(1)
    );
}

#[test]
fn unavailable_projection_clears_completeness_until_successful_refresh() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    let root_string = root.to_string();
    let projection = app_projection(
        &root_string,
        vec![app_projection_source(&root_string, "first", 10)],
    );

    rollup
        .observe_app_projection(&root_string, &projection)
        .expect("valid projection should merge");
    assert!(rollup.snapshot_for(root).complete);
    assert_eq!(rollup.snapshot_for(root).total_usage, usage(10));

    rollup
        .observe_app_projection_unavailable(&root_string)
        .expect("valid root id should mark projection unavailable");
    let unavailable = rollup.snapshot_for(root);
    assert!(!unavailable.complete);
    assert_eq!(unavailable.total_usage, usage(10));

    rollup
        .observe_app_projection(&root_string, &projection)
        .expect("valid refresh should merge");
    assert!(rollup.snapshot_for(root).complete);
    assert_eq!(rollup.snapshot_for(root).total_usage, usage(10));
}

#[test]
fn malformed_projection_does_not_leave_partial_edges_or_completeness() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    let root_string = root.to_string();
    let projection = app_projection(
        &root_string,
        vec![
            app_projection_source(&root_string, "valid", 10),
            app_projection_source("not-a-thread-id", "invalid", 20),
        ],
    );

    assert!(
        rollup
            .observe_app_projection(&root_string, &projection)
            .is_err()
    );
    assert_eq!(rollup.snapshot_for(root), UsageRollupSnapshot::default());
}

#[test]
fn app_projection_keeps_empty_edges_and_authoritative_zero() {
    let (mut rollup, _temp_dir) = new_rollup();
    let root = ThreadId::new();
    let middle = ThreadId::new();
    let leaf = ThreadId::new();
    let projection = codex_app_server_protocol::ThreadTokenUsageProjection {
        total: codex_app_server_protocol::TokenUsageBreakdown {
            total_tokens: 23,
            input_tokens: 23,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
        },
        threads: vec![
            codex_app_server_protocol::ThreadTokenUsageProjectionThread {
                thread_id: root.to_string(),
                parent_thread_id: None,
                forked_from_id: None,
                sources: Vec::new(),
                response_ids: Vec::new(),
            },
            codex_app_server_protocol::ThreadTokenUsageProjectionThread {
                thread_id: middle.to_string(),
                parent_thread_id: Some(root.to_string()),
                forked_from_id: None,
                sources: Vec::new(),
                response_ids: Vec::new(),
            },
            codex_app_server_protocol::ThreadTokenUsageProjectionThread {
                thread_id: leaf.to_string(),
                parent_thread_id: Some(middle.to_string()),
                forked_from_id: None,
                sources: vec![codex_app_server_protocol::ThreadTokenUsageSource {
                    thread_id: leaf.to_string(),
                    attribution: codex_app_server_protocol::ThreadTokenUsageAttribution {
                        model: Some("model-a".to_string()),
                        model_provider: Some("openai".to_string()),
                        service_tier: Some("standard".to_string()),
                        context_length: Some("short".to_string()),
                    },
                    response_ids: vec!["leaf-response".to_string()],
                    usage: codex_app_server_protocol::TokenUsageBreakdown {
                        total_tokens: 23,
                        input_tokens: 23,
                        cached_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        output_tokens: 0,
                        reasoning_output_tokens: 0,
                    },
                }],
                response_ids: vec![
                    codex_app_server_protocol::ThreadTokenUsageResponseIdentity {
                        thread_id: leaf.to_string(),
                        response_id: "leaf-response".to_string(),
                    },
                ],
            },
        ],
    };

    rollup
        .observe_app_projection(&root.to_string(), &projection)
        .expect("projection should parse");
    let snapshot = rollup.snapshot_for(root);

    assert!(snapshot.complete);
    assert_eq!(snapshot.total_usage, usage(23));
    assert_eq!(snapshot.sources.len(), 1);
    assert_eq!(snapshot.sources[0].thread_id, leaf);

    let empty_fork = ThreadId::new();
    let empty_fork_projection = codex_app_server_protocol::ThreadTokenUsageProjection {
        total: codex_app_server_protocol::TokenUsageBreakdown {
            total_tokens: 0,
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
        },
        threads: vec![
            codex_app_server_protocol::ThreadTokenUsageProjectionThread {
                thread_id: empty_fork.to_string(),
                parent_thread_id: Some(root.to_string()),
                forked_from_id: Some(root.to_string()),
                sources: Vec::new(),
                response_ids: Vec::new(),
            },
        ],
    };
    rollup
        .observe_app_projection(&empty_fork.to_string(), &empty_fork_projection)
        .expect("fork projection should parse");
    let fork_snapshot = rollup.snapshot_for(empty_fork);
    assert!(fork_snapshot.complete);
    assert_eq!(fork_snapshot.total_usage, TokenUsage::default());
}
