ALTER TABLE thread_activity_pauses
ADD COLUMN snapshot_captured INTEGER NOT NULL DEFAULT 0;

CREATE TABLE thread_activity_pause_snapshots (
    root_thread_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    thread_id TEXT NOT NULL,
    parent_thread_id TEXT,
    PRIMARY KEY (root_thread_id, generation, thread_id)
);

CREATE INDEX idx_thread_activity_pause_snapshots_root_generation
ON thread_activity_pause_snapshots (root_thread_id, generation);

CREATE TABLE thread_activity_pause_receipts (
    root_thread_id TEXT PRIMARY KEY NOT NULL,
    generation INTEGER NOT NULL,
    completed_at_ms INTEGER NOT NULL
);
