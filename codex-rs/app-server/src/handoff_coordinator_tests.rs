use super::api_node_from_core;
use super::node_depth;
use super::ordered_indices;
use super::receipt_from_journal;
use codex_app_server_protocol::ThreadHandoffNodeState;
use codex_core::{HandoffJournal, HandoffJournalState, HandoffNode, HandoffNodeState};
use pretty_assertions::assert_eq;

fn node(
    thread_id: &str,
    parent_thread_id: Option<&str>,
    state: HandoffNodeState,
    turn_id: Option<&str>,
) -> HandoffNode {
    HandoffNode {
        thread_id: thread_id.to_string(),
        root_thread_id: "root".to_string(),
        parent_thread_id: parent_thread_id.map(str::to_string),
        agent_path: None,
        turn_id: turn_id.map(str::to_string),
        rollout_path: None,
        was_running: turn_id.is_some(),
        was_paused: false,
        state,
        blockers: Vec::new(),
    }
}

#[test]
fn suspension_orders_children_before_parent() {
    let nodes = vec![
        node("root", None, HandoffNodeState::Planned, Some("root-turn")),
        node(
            "child",
            Some("root"),
            HandoffNodeState::Planned,
            Some("child-turn"),
        ),
    ];

    assert_eq!(ordered_indices(&nodes, true), vec![1, 0]);
}

#[test]
fn recovery_orders_parent_before_child() {
    let nodes = vec![
        node("root", None, HandoffNodeState::Suspended, Some("root-turn")),
        node(
            "child",
            Some("root"),
            HandoffNodeState::Suspended,
            Some("child-turn"),
        ),
    ];

    assert_eq!(ordered_indices(&nodes, false), vec![0, 1]);
}

#[test]
fn node_depth_ignores_missing_parent() {
    let nodes = vec![node(
        "child",
        Some("missing"),
        HandoffNodeState::Suspended,
        Some("turn"),
    )];
    let mut depths = vec![None];

    assert_eq!(
        node_depth(
            0,
            &nodes,
            &mut depths,
            &mut std::collections::HashSet::new()
        ),
        0
    );
}

#[test]
fn idle_suspension_maps_to_not_active() {
    let node = node("root", None, HandoffNodeState::Suspended, None);

    let api_node = api_node_from_core(&node);

    assert_eq!(api_node.state, ThreadHandoffNodeState::NotActive);
    assert_eq!(api_node.turn_id, None);
    assert_eq!(api_node.blockers, Vec::new());
}

#[test]
fn receipt_converts_milliseconds_to_unix_seconds() {
    let journal = HandoffJournal {
        schema_version: 1,
        handoff_id: "handoff-1".to_string(),
        created_at_ms: 12_345,
        runtime_version: "test".to_string(),
        state: HandoffJournalState::Suspended,
        nodes: vec![node(
            "root",
            None,
            HandoffNodeState::Suspended,
            Some("turn"),
        )],
    };

    let receipt = receipt_from_journal(&journal);

    assert_eq!(receipt.created_at, 12);
    assert_eq!(
        receipt.state,
        codex_app_server_protocol::ThreadHandoffState::Suspended
    );
    assert_eq!(receipt.nodes.len(), 1);
}
