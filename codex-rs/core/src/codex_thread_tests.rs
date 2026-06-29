use super::with_mcp_tool_call_codex_meta;
use pretty_assertions::assert_eq;

#[test]
fn mcp_tool_call_codex_meta_is_added_to_request_meta() {
    assert_eq!(
        with_mcp_tool_call_codex_meta(
            Some(serde_json::json!({
                "source": "test-client",
                "threadId": "stale-thread",
                "codex": {
                    "threadId": "stale-thread",
                    "sessionId": "stale-session",
                    "cwd": "/stale",
                },
            })),
            "thread-live",
            "session-live",
            "/workspace/project",
        ),
        Some(serde_json::json!({
            "source": "test-client",
            "threadId": "thread-live",
            "codex": {
                "threadId": "thread-live",
                "sessionId": "session-live",
                "cwd": "/workspace/project",
            },
        }))
    );
}

#[test]
fn mcp_tool_call_codex_meta_is_created_for_empty_request_meta() {
    assert_eq!(
        with_mcp_tool_call_codex_meta(
            /*meta*/ None,
            "thread-live",
            "session-live",
            "/workspace/project",
        ),
        Some(serde_json::json!({
            "threadId": "thread-live",
            "codex": {
                "threadId": "thread-live",
                "sessionId": "session-live",
                "cwd": "/workspace/project",
            },
        }))
    );
}
