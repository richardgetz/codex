-- Keep the task-first All Sessions query and terminal-history pruning bounded without
-- changing the existing per-root ETA indexes.
CREATE INDEX eta_tasks_cross_session_order_idx
    ON eta_tasks (updated_at DESC, root_thread_id, task_id);

CREATE INDEX eta_tasks_terminal_retention_idx
    ON eta_tasks (terminal_at, root_thread_id, task_id)
    WHERE status IN ('completed', 'cancelled') AND terminal_at IS NOT NULL;

CREATE INDEX eta_tasks_parent_idx
    ON eta_tasks (root_thread_id, parent_task_id, task_id);
