use super::api_node_from_core;
use super::node_depth;
use super::ordered_indices;
use super::quarantine::validate_graph;
use super::receipt_from_journal;
use codex_app_server_protocol::ThreadHandoffNodeState;
use codex_core::HandoffJournal;
use codex_core::HandoffJournalState;
use codex_core::HandoffNode;
use codex_core::HandoffNodeState;
use codex_protocol::ThreadId;
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

fn graph_node(
    thread_id: &str,
    root_thread_id: &str,
    parent_thread_id: Option<&str>,
) -> HandoffNode {
    HandoffNode {
        thread_id: thread_id.to_string(),
        root_thread_id: root_thread_id.to_string(),
        parent_thread_id: parent_thread_id.map(str::to_string),
        agent_path: None,
        turn_id: Some("turn".to_string()),
        rollout_path: None,
        was_running: true,
        was_paused: false,
        state: HandoffNodeState::Suspended,
        blockers: Vec::new(),
    }
}

fn graph_journal(nodes: Vec<HandoffNode>) -> HandoffJournal {
    HandoffJournal {
        schema_version: 1,
        handoff_id: "handoff-graph".to_string(),
        created_at_ms: 0,
        runtime_version: "test".to_string(),
        state: HandoffJournalState::NeedsAttention,
        transfer_started: Some(true),
        quarantined: false,
        nodes,
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
fn graph_validation_rejects_long_parent_cycles() {
    let root = ThreadId::new().to_string();
    let first = ThreadId::new().to_string();
    let second = ThreadId::new().to_string();
    let journal = graph_journal(vec![
        graph_node(&first, &root, Some(&second)),
        graph_node(&second, &root, Some(&first)),
    ]);

    let error = validate_graph(&journal, false).expect_err("cycle must remain fenced");
    assert!(error.message.contains("parent cycle"));
}

#[test]
fn graph_validation_requires_complete_parent_closure_for_recovery() {
    let root = ThreadId::new().to_string();
    let child = ThreadId::new().to_string();
    let missing_parent = ThreadId::new().to_string();
    let journal = graph_journal(vec![graph_node(&child, &root, Some(&missing_parent))]);

    assert!(validate_graph(&journal, true).is_err());
    assert!(validate_graph(&journal, false).is_ok());
}

#[test]
fn graph_validation_rejects_known_cross_root_parent() {
    let first_root = ThreadId::new().to_string();
    let second_root = ThreadId::new().to_string();
    let first = ThreadId::new().to_string();
    let second = ThreadId::new().to_string();
    let journal = graph_journal(vec![
        graph_node(&first, &first_root, Some(&second)),
        graph_node(&second, &second_root, None),
    ]);

    let error = validate_graph(&journal, false).expect_err("cross-root parent must be fenced");
    assert!(error.message.contains("inconsistent root lineage"));
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
        transfer_started: Some(true),
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
