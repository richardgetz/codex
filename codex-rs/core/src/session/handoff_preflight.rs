//! Safe-boundary checks for cross-process daemon handoff.
//!
//! A regular model stream can be cancelled and resumed under its original turn
//! ID. Tool/MCP work, host callbacks, pending queues, and non-regular tasks do
//! not have that guarantee; callers must leave the old owner alive and surface
//! the blocker instead of replaying it.

use super::session::Session;
use crate::HandoffBlocker;
use crate::state::TaskKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HandoffDescendantState {
    MustBeDrained,
    AlreadyDrained,
}

/// Process-local facts captured immediately before suspending one thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffPreflight {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub was_running: bool,
    pub was_paused: bool,
    pub blockers: Vec<HandoffBlocker>,
}

impl Session {
    /// Inspect the current thread without changing its turn or queue state.
    pub(crate) async fn handoff_preflight(&self) -> HandoffPreflight {
        self.handoff_preflight_with_descendants(HandoffDescendantState::MustBeDrained)
            .await
    }

    /// Inspect a node whose loaded descendants have already been suspended and durably recorded.
    pub(crate) async fn handoff_preflight_after_descendants(&self) -> HandoffPreflight {
        self.handoff_preflight_with_descendants(HandoffDescendantState::AlreadyDrained)
            .await
    }

    async fn handoff_preflight_with_descendants(
        &self,
        descendants: HandoffDescendantState,
    ) -> HandoffPreflight {
        let was_paused = self.is_activity_paused();
        let mut preflight = HandoffPreflight {
            thread_id: self.thread_id.to_string(),
            turn_id: None,
            was_running: false,
            was_paused,
            blockers: Vec::new(),
        };

        {
            let active = self.active_turn.lock().await;
            if let Some(active_turn) = active.as_ref() {
                let turn_state = active_turn.turn_state.lock().await;
                if turn_state.has_pending_approval() {
                    preflight.blockers.push(HandoffBlocker::PendingApproval);
                }
                if turn_state.has_pending_user_input() {
                    preflight.blockers.push(HandoffBlocker::PendingUserInput);
                }
                if turn_state.has_pending_dynamic_tools() {
                    preflight
                        .blockers
                        .push(HandoffBlocker::PendingDynamicToolResponse);
                }
                if turn_state.has_pending_input() {
                    preflight.blockers.push(HandoffBlocker::PendingInput);
                }
                if let Some(task) = active_turn.task.as_ref() {
                    preflight.was_running = true;
                    preflight.turn_id = Some(task.turn_context.sub_id.clone());
                    if task.kind != TaskKind::Regular {
                        preflight.blockers.push(HandoffBlocker::UnsupportedTask);
                    }
                }
            }
        }

        if self.input_queue.has_pending_mailbox_items().await {
            preflight.blockers.push(HandoffBlocker::PendingMailbox);
        }
        if self.pending_handoff_dispatches() > 0 {
            preflight.blockers.push(HandoffBlocker::PendingDispatch);
        }
        if self.non_model_activity_in_flight() > 0 {
            preflight.blockers.push(HandoffBlocker::ActiveOperation);
        }
        if self
            .turn_finalization_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
        {
            preflight.blockers.push(HandoffBlocker::TurnFinalization);
        }
        if self.conversation.running_state().await.is_some() {
            preflight
                .blockers
                .push(HandoffBlocker::RealtimeConversation);
        }
        if !self.list_background_terminals().await.is_empty() {
            preflight.blockers.push(HandoffBlocker::UnifiedExecProcess);
        }
        if descendants == HandoffDescendantState::MustBeDrained
            && self
                .services
                .agent_control
                .list_live_agent_subtree_thread_ids(self.thread_id)
                .await
                .map(|thread_ids| thread_ids.len() > 1)
                .unwrap_or(true)
        {
            preflight.blockers.push(HandoffBlocker::LiveDescendants);
        }

        preflight
    }
}
