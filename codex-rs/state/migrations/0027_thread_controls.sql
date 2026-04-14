CREATE TABLE thread_controls (
    thread_id TEXT NOT NULL PRIMARY KEY,
    mode TEXT NOT NULL,
    reason TEXT NOT NULL,
    release_channel TEXT,
    watch_interval_seconds INTEGER,
    released_at INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE TABLE thread_control_targets (
    thread_id TEXT NOT NULL,
    target_thread_id TEXT NOT NULL,
    PRIMARY KEY (thread_id, target_thread_id)
);

CREATE INDEX idx_thread_controls_mode_released
    ON thread_controls(mode, released_at);
