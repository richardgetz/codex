use super::*;

use pretty_assertions::assert_eq;

fn usage(input_tokens: i64, output_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens + output_tokens,
        ..TokenUsage::default()
    }
}

fn record(
    thread_id: ThreadId,
    parent_thread_id: Option<ThreadId>,
    response_id: &str,
    usage: TokenUsage,
    attribution: TokenUsageAttribution,
) -> TokenUsageRecord {
    TokenUsageRecord {
        thread_id,
        parent_thread_id,
        turn_id: "turn-1".to_string(),
        session_id: SessionId::from(thread_id),
        root_turn_id: "turn-1".to_string(),
        response_id: response_id.to_string(),
        turn_token_usage: usage.clone(),
        thread_token_usage: usage.clone(),
        usage,
        attribution,
        completed_at_ms: Some(1_000),
    }
}

#[test]
fn token_usage_projection_deduplicates_exact_responses_and_keeps_empty_threads() {
    let root_thread_id = ThreadId::new();
    let child_thread_id = ThreadId::new();
    let empty_thread_id = ThreadId::new();
    let short = TokenUsageAttribution {
        model: Some("gpt-5.4".to_string()),
        model_provider: Some("openai".to_string()),
        service_tier: Some("standard".to_string()),
        context_length: Some("short".to_string()),
    };
    let long = TokenUsageAttribution {
        context_length: Some("long".to_string()),
        ..short.clone()
    };
    let first = usage(10, 2);
    let duplicate = usage(100, 100);
    let second = usage(20, 3);
    let child_usage = usage(7, 1);
    let mut expected_total = first.clone();
    expected_total.add_assign(&second);
    expected_total.add_assign(&child_usage);

    let projection = TokenUsageProjection::from_threads_and_records(
        [
            TokenUsageProjectionThread {
                thread_id: root_thread_id,
                ..Default::default()
            },
            TokenUsageProjectionThread {
                thread_id: child_thread_id,
                parent_thread_id: Some(root_thread_id),
                ..Default::default()
            },
            TokenUsageProjectionThread {
                thread_id: empty_thread_id,
                parent_thread_id: Some(child_thread_id),
                ..Default::default()
            },
        ],
        [
            record(
                root_thread_id,
                None,
                "response-1",
                first.clone(),
                short.clone(),
            ),
            record(root_thread_id, None, "response-1", duplicate, long.clone()),
            record(root_thread_id, None, "response-2", second, long),
            record(
                child_thread_id,
                Some(root_thread_id),
                "child-response",
                child_usage,
                short,
            ),
        ],
    );

    assert_eq!(projection.total_usage, expected_total);
    assert_eq!(projection.threads.len(), 3);
    let root = projection
        .threads
        .iter()
        .find(|thread| thread.thread_id == root_thread_id)
        .expect("root projection thread");
    assert_eq!(root.response_ids.len(), 2);
    assert_eq!(root.sources.len(), 2);
    assert!(projection.threads.iter().any(|thread| {
        thread.thread_id == empty_thread_id
            && thread.parent_thread_id == Some(child_thread_id)
            && thread.sources.is_empty()
            && thread.response_ids.is_empty()
    }));
}
