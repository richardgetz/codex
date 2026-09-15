CREATE TABLE thread_activity_pauses (
    root_thread_id TEXT PRIMARY KEY NOT NULL,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pausing', 'paused', 'resuming')),
    updated_at_ms INTEGER NOT NULL
);
