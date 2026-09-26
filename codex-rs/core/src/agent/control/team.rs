use super::LocalAgentControl;
use crate::agent::status::is_final;
use crate::agent::types::AgentMetadata;
use crate::session::Session;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Agents closed by a session-scoped idle prune, plus roots that failed to close.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PruneIdleAgentsReport {
    pub(crate) closed: Vec<ThreadId>,
    pub(crate) failed: Vec<(ThreadId, String)>,
}

/// Holds a terminal result obligation from child startup until its legacy watcher is installed.
pub(crate) struct TerminalResultDeliveryGuard {
    session: Arc<Session>,
    parent_thread_id: ThreadId,
}

impl TerminalResultDeliveryGuard {
    pub(crate) fn for_thread_spawn(
        session: Arc<Session>,
        session_source: Option<&SessionSource>,
    ) -> Option<Self> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };
        session
            .terminal_result_delivery_in_flight
            .fetch_add(1, Ordering::AcqRel);
        Some(Self {
            session,
            parent_thread_id: *parent_thread_id,
        })
    }
}

impl Drop for TerminalResultDeliveryGuard {
    fn drop(&mut self) {
        let previous = self
            .session
            .terminal_result_delivery_in_flight
            .fetch_sub(1, Ordering::AcqRel);
        if previous == 1
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let agent_control = self.session.services.agent_control.clone();
            let parent_thread_id = self.parent_thread_id;
            runtime.spawn(async move {
                agent_control
                    .schedule_pending_manager_completion_batch_flush(parent_thread_id)
                    .await;
            });
        }
    }
}

impl LocalAgentControl {
    /// Counts direct Worker children that can still perform work for a parent session.
    /// Terminal children remain active until their parent result callback has been delivered.
    pub(crate) async fn active_direct_worker_count(&self, parent_thread_id: ThreadId) -> usize {
        let Ok(state) = self.upgrade() else {
            return 0;
        };
        let Ok(children) = self.open_thread_spawn_children(parent_thread_id).await else {
            return 0;
        };
        let mut active = 0;
        for (thread_id, _) in children {
            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            let status = thread.agent_status().await;
            let terminal_delivery_in_flight = is_final(&status)
                && thread
                    .session
                    .terminal_result_delivery_in_flight
                    .load(Ordering::Acquire)
                    > 0;
            if matches!(status, AgentStatus::PendingInit | AgentStatus::Running)
                || terminal_delivery_in_flight
            {
                active += 1;
            }
        }
        active
    }

    /// Rearms a pending manager-only completion batch after a child finishes delivering its
    /// terminal result. A prior quiet-window flush may have observed that delivery in flight.
    pub(crate) async fn schedule_pending_manager_completion_batch_flush(
        &self,
        parent_thread_id: ThreadId,
    ) {
        let Ok(state) = self.upgrade() else {
            return;
        };
        let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
            return;
        };
        if let Some(generation) = parent_thread
            .session
            .input_queue
            .pending_manager_completion_generation()
            .await
        {
            parent_thread
                .session
                .schedule_manager_completion_batch_flush(generation)
                .await;
        }
    }

    /// Returns whether a target thread currently has the Lead assignment.
    pub(crate) async fn parent_is_team_lead(&self, thread_id: ThreadId) -> bool {
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let Ok(thread) = state.get_thread(thread_id).await else {
            return false;
        };
        thread.session.is_team_lead().await
    }

    /// Close idle agent subtrees in the current session, leaving any subtree containing
    /// active work untouched.
    pub(crate) async fn prune_idle_agents(
        &self,
        current_thread_id: ThreadId,
    ) -> CodexResult<PruneIdleAgentsReport> {
        let _admission = self.begin_handoff_admission()?;
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        for children in children_by_parent.values_mut() {
            children.sort_by_key(|left| left.0.to_string());
        }
        let mut parent_by_child = HashMap::new();
        for (parent_thread_id, children) in &children_by_parent {
            for (child_thread_id, _) in children {
                parent_by_child.insert(*child_thread_id, *parent_thread_id);
            }
        }

        let mut session_root_thread_id = current_thread_id;
        let mut visited_ancestors = HashSet::new();
        while let Some(parent_thread_id) = parent_by_child.get(&session_root_thread_id).copied() {
            if !visited_ancestors.insert(parent_thread_id) {
                break;
            }
            session_root_thread_id = parent_thread_id;
        }

        let mut session_thread_ids = HashSet::from([session_root_thread_id]);
        session_thread_ids.extend(collect_descendants(
            session_root_thread_id,
            &children_by_parent,
        ));

        let mut thread_spawn_depths = HashMap::new();
        let mut depth_queue = VecDeque::from([(session_root_thread_id, 0usize)]);
        while let Some((thread_id, depth)) = depth_queue.pop_front() {
            if thread_spawn_depths.insert(thread_id, depth).is_some() {
                continue;
            }
            if let Some(children) = children_by_parent.get(&thread_id) {
                for (child_thread_id, _) in children {
                    depth_queue.push_back((*child_thread_id, depth.saturating_add(1)));
                }
            }
        }

        let mut live_agents = self.runtime.registry.live_agents();
        let mut live_agent_ids = HashSet::new();
        live_agents.retain(|metadata| {
            metadata
                .agent_id
                .is_some_and(|thread_id| live_agent_ids.insert(thread_id))
        });
        for children in children_by_parent.values() {
            for (thread_id, metadata) in children {
                if !session_thread_ids.contains(thread_id) || !live_agent_ids.insert(*thread_id) {
                    continue;
                }
                let mut metadata = metadata.clone();
                metadata.agent_id = Some(*thread_id);
                live_agents.push(metadata);
            }
        }
        live_agents.sort_by(|left, right| {
            let left_depth = left
                .agent_id
                .and_then(|thread_id| thread_spawn_depths.get(&thread_id).copied())
                .unwrap_or(usize::MAX);
            let right_depth = right
                .agent_id
                .and_then(|thread_id| thread_spawn_depths.get(&thread_id).copied())
                .unwrap_or(usize::MAX);
            left_depth
                .cmp(&right_depth)
                .then_with(|| {
                    left.agent_path
                        .as_deref()
                        .unwrap_or_default()
                        .cmp(right.agent_path.as_deref().unwrap_or_default())
                })
                .then_with(|| {
                    left.agent_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .cmp(&right.agent_id.map(|id| id.to_string()).unwrap_or_default())
                })
        });

        let protected = HashSet::from([current_thread_id]);
        let mut handled = HashSet::new();
        let mut report = PruneIdleAgentsReport::default();
        for metadata in live_agents {
            let Some(thread_id) = metadata.agent_id else {
                continue;
            };
            if handled.contains(&thread_id) {
                continue;
            }

            let descendants = collect_descendants(thread_id, &children_by_parent);
            if std::iter::once(thread_id)
                .chain(descendants.iter().copied())
                .any(|candidate| protected.contains(&candidate))
            {
                continue;
            }

            let mut subtree_contains_active_work = false;
            for candidate in std::iter::once(thread_id).chain(descendants.iter().copied()) {
                if matches!(
                    self.get_status(candidate).await,
                    AgentStatus::PendingInit | AgentStatus::Running
                ) {
                    subtree_contains_active_work = true;
                    break;
                }
            }
            if subtree_contains_active_work {
                continue;
            }

            let mut subtree = vec![thread_id];
            subtree.extend(descendants);
            let unhandled_subtree = subtree
                .into_iter()
                .filter(|candidate| !handled.contains(candidate))
                .collect::<Vec<_>>();
            match self.close_agent(thread_id).await {
                Ok(_) => {
                    handled.extend(unhandled_subtree.iter().copied());
                    report.closed.extend(unhandled_subtree);
                }
                Err(err)
                    if matches!(
                        err.details(),
                        CodexErrorDetails::ThreadNotFound(_) | CodexErrorDetails::InternalAgentDied
                    ) =>
                {
                    handled.extend(unhandled_subtree.iter().copied());
                    report.closed.extend(unhandled_subtree);
                }
                Err(err) => {
                    handled.insert(thread_id);
                    report.failed.push((thread_id, err.to_string()));
                }
            }
        }

        report.closed.sort_by_key(std::string::ToString::to_string);
        report
            .failed
            .sort_by_key(|(thread_id, _)| thread_id.to_string());
        Ok(report)
    }
}

fn collect_descendants(
    root_thread_id: ThreadId,
    children_by_parent: &HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>,
) -> Vec<ThreadId> {
    let mut descendants = Vec::new();
    let mut visited = HashSet::new();
    let mut stack = children_by_parent
        .get(&root_thread_id)
        .into_iter()
        .flatten()
        .map(|(thread_id, _)| *thread_id)
        .rev()
        .collect::<Vec<_>>();
    while let Some(thread_id) = stack.pop() {
        if !visited.insert(thread_id) {
            continue;
        }
        descendants.push(thread_id);
        if let Some(children) = children_by_parent.get(&thread_id) {
            stack.extend(children.iter().rev().map(|(child_thread_id, _)| *child_thread_id));
        }
    }
    descendants
}
