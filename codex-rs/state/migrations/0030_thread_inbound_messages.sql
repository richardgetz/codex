CREATE TABLE thread_inbound_messages (
    id TEXT PRIMARY KEY,
    target_thread_id TEXT NOT NULL,
    source_thread_id TEXT,
    payload_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    delivered_at_ms INTEGER,
    FOREIGN KEY(target_thread_id) REFERENCES threads(id) ON DELETE CASCADE
);

CREATE INDEX idx_thread_inbound_messages_target_pending
    ON thread_inbound_messages(target_thread_id, delivered_at_ms, created_at_ms);
