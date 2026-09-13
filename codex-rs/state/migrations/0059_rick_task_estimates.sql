CREATE TABLE eta_roots (
    root_thread_id TEXT PRIMARY KEY NOT NULL,
    sequence INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE eta_tasks (
    task_id TEXT NOT NULL,
    root_thread_id TEXT NOT NULL,
    owner_thread_id TEXT NOT NULL,
    parent_task_id TEXT,
    depends_on_task_ids TEXT NOT NULL DEFAULT '[]',
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    current_lower_seconds INTEGER,
    current_upper_seconds INTEGER,
    original_lower_seconds INTEGER,
    original_upper_seconds INTEGER,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    terminal_at INTEGER,
    actual_elapsed_seconds INTEGER,
    updated_at INTEGER NOT NULL,
    updated_sequence INTEGER NOT NULL,
    PRIMARY KEY (root_thread_id, task_id),
    FOREIGN KEY (root_thread_id) REFERENCES eta_roots(root_thread_id)
);

CREATE INDEX eta_tasks_root_status_idx
    ON eta_tasks (root_thread_id, status, task_id);

CREATE INDEX eta_tasks_root_terminal_idx
    ON eta_tasks (root_thread_id, terminal_at, task_id);

CREATE TABLE eta_task_revisions (
    root_thread_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    lower_seconds INTEGER,
    upper_seconds INTEGER,
    reason TEXT,
    updated_at INTEGER NOT NULL,
    actor_thread_id TEXT NOT NULL,
    PRIMARY KEY (root_thread_id, task_id, revision),
    FOREIGN KEY (root_thread_id, task_id)
        REFERENCES eta_tasks(root_thread_id, task_id) ON DELETE CASCADE
);

CREATE INDEX eta_task_revisions_task_idx
    ON eta_task_revisions (root_thread_id, task_id, revision);
