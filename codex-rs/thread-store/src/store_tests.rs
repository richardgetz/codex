use super::*;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionConfiguredEvent;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageAttribution;
use codex_protocol::protocol::TokenUsageRecord;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;

fn record(thread_id: ThreadId, response_id: &str) -> TokenUsageRecord {
    let usage = TokenUsage {
        input_tokens: 8,
        output_tokens: 2,
        total_tokens: 10,
        ..Default::default()
    };
    TokenUsageRecord {
        thread_id,
        parent_thread_id: None,
        turn_id: "turn-1".to_string(),
        session_id: SessionId::from(thread_id),
        root_turn_id: "turn-1".to_string(),
        response_id: response_id.to_string(),
        usage: usage.clone(),
        turn_token_usage: usage.clone(),
        thread_token_usage: usage,
        attribution: TokenUsageAttribution::default(),
        completed_at_ms: None,
    }
}

#[test]
fn usage_records_include_compaction_checkpoint_records() {
    let thread_id = ThreadId::new();
    let direct = record(thread_id, "direct");
    let checkpoint = record(thread_id, "checkpoint");
    let items = vec![
        RolloutItem::TokenUsageRecord(direct.clone()),
        RolloutItem::Compacted(CompactedItem {
            message: "checkpoint".to_string(),
            replacement_history: None,
            guardian_history: None,
            mcp_resource_origins: None,
            window_number: None,
            first_window_id: None,
            previous_window_id: None,
            window_id: None,
            compaction_response_id: None,
            latest_token_usage_record: Some(checkpoint.clone()),
        }),
    ];

    assert_eq!(
        token_usage_records_from_rollout_items(&items),
        vec![direct, checkpoint]
    );
}

#[test]
fn legacy_usage_records_use_historical_provider_and_leave_model_unknown() {
    let thread_id = ThreadId::new();
    let mut legacy = serde_json::to_value(record(thread_id, "legacy"))
        .expect("serialize legacy usage record fixture");
    legacy
        .as_object_mut()
        .expect("legacy usage record object")
        .remove("attribution");

    let legacy_record: TokenUsageRecord =
        serde_json::from_value(legacy).expect("deserialize legacy usage record");
    let mut expected = record(thread_id, "legacy");
    expected.attribution = TokenUsageAttribution {
        model: None,
        model_provider: Some("historical-provider".to_string()),
        service_tier: None,
        context_length: Some("short".to_string()),
    };

    let records = enrich_token_usage_records(vec![legacy_record], &[], Some("historical-provider"));

    assert_eq!(records, vec![expected]);
}

fn session_configured(
    thread_id: ThreadId,
    model: &str,
    model_provider_id: &str,
    service_tier: Option<&str>,
) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::SessionConfigured(SessionConfiguredEvent {
        session_id: SessionId::from(thread_id),
        thread_id,
        forked_from_id: None,
        parent_thread_id: None,
        thread_source: None,
        thread_name: None,
        model: model.to_string(),
        model_provider_id: model_provider_id.to_string(),
        service_tier: service_tier.map(str::to_string),
        approval_policy: AskForApproval::Never,
        approvals_reviewer: ApprovalsReviewer::User,
        permission_profile: PermissionProfile::read_only(),
        active_permission_profile: None,
        cwd: AbsolutePathBuf::from_absolute_path(Path::new("/tmp"))
            .expect("test cwd should be absolute"),
        reasoning_effort: None,
        initial_messages: None,
        network_proxy: None,
        rollout_path: None,
    }))
}

#[test]
fn usage_enrichment_uses_record_position_instead_of_final_session_settings() {
    let thread_id = ThreadId::new();
    let mut old_record = record(thread_id, "old-response");
    old_record.turn_id = "missing-turn".to_string();
    let mut expected = old_record.clone();
    expected.attribution = TokenUsageAttribution {
        model: Some("old-model".to_string()),
        model_provider: Some("old-provider".to_string()),
        service_tier: Some("old-tier".to_string()),
        context_length: Some("short".to_string()),
    };
    let items = vec![
        session_configured(thread_id, "old-model", "old-provider", Some("old-tier")),
        RolloutItem::TokenUsageRecord(old_record.clone()),
        session_configured(thread_id, "new-model", "new-provider", Some("new-tier")),
    ];

    assert_eq!(
        enrich_token_usage_records(vec![old_record], &items, None),
        vec![expected]
    );
}

#[test]
fn usage_enrichment_preserves_missing_service_tier_on_attributed_record() {
    let thread_id = ThreadId::new();
    let mut attributed = record(thread_id, "attributed-response");
    attributed.attribution = TokenUsageAttribution {
        model: Some("current-model".to_string()),
        model_provider: Some("current-provider".to_string()),
        service_tier: None,
        context_length: Some("short".to_string()),
    };
    let items = vec![
        session_configured(
            thread_id,
            "historical-model",
            "historical-provider",
            Some("historical-tier"),
        ),
        RolloutItem::TokenUsageRecord(attributed.clone()),
    ];

    assert_eq!(
        enrich_token_usage_records(vec![attributed.clone()], &items, None),
        vec![attributed]
    );
}

#[test]
fn usage_enrichment_uses_each_rollout_segment_session_metadata() {
    let thread_id = ThreadId::new();
    let old_record = record(thread_id, "old-response");
    let new_record = record(thread_id, "new-response");

    let mut old_meta = SessionMeta::default();
    old_meta.id = thread_id;
    old_meta.session_id = SessionId::from(thread_id);
    old_meta.model_provider = Some("old-provider".to_string());
    let mut new_meta = old_meta.clone();
    new_meta.model_provider = Some("new-provider".to_string());
    let items = vec![
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: old_meta,
            git: None,
        }),
        RolloutItem::TokenUsageRecord(old_record.clone()),
        RolloutItem::SessionMeta(SessionMetaLine {
            meta: new_meta,
            git: None,
        }),
        RolloutItem::TokenUsageRecord(new_record.clone()),
    ];

    let mut expected_old = old_record.clone();
    expected_old.attribution = TokenUsageAttribution {
        model_provider: Some("old-provider".to_string()),
        context_length: Some("short".to_string()),
        ..TokenUsageAttribution::default()
    };
    let mut expected_new = new_record.clone();
    expected_new.attribution = TokenUsageAttribution {
        model_provider: Some("new-provider".to_string()),
        context_length: Some("short".to_string()),
        ..TokenUsageAttribution::default()
    };

    assert_eq!(
        enrich_token_usage_records(vec![old_record, new_record], &items, None),
        vec![expected_old, expected_new]
    );
}
