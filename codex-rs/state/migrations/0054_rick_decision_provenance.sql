-- Codex-native decision provenance. The event table is append-only; the
-- remaining tables are materialized, indexed views of the latest state.
CREATE TABLE provenance_events (
    event_id TEXT PRIMARY KEY,
    schema_version INTEGER NOT NULL,
    idempotency_key TEXT UNIQUE,
    event_type TEXT NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL,
    actor TEXT NOT NULL,
    privacy_class TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_provenance_events_aggregate
    ON provenance_events(aggregate_type, aggregate_id, occurred_at_ms, event_id);
CREATE INDEX idx_provenance_events_occurred_at
    ON provenance_events(occurred_at_ms, event_id);

CREATE TABLE preference_boundaries (
    id TEXT PRIMARY KEY,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    strength TEXT NOT NULL,
    authority TEXT NOT NULL,
    lifecycle_status TEXT NOT NULL,
    related_memory_record_id TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE INDEX idx_preference_boundaries_scope
    ON preference_boundaries(scope_kind, scope_id, lifecycle_status);
CREATE INDEX idx_preference_boundaries_status
    ON preference_boundaries(lifecycle_status, updated_at_ms, id);
CREATE INDEX idx_preference_boundaries_memory
    ON preference_boundaries(related_memory_record_id);

CREATE TABLE crossroads (
    id TEXT PRIMARY KEY,
    request_ref TEXT,
    task_ref TEXT,
    project_ref TEXT,
    session_id TEXT,
    status TEXT NOT NULL,
    actor TEXT NOT NULL,
    authority_required TEXT,
    linked_scratchpad_wait_id TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE INDEX idx_crossroads_session_status
    ON crossroads(session_id, status, updated_at_ms, id);
CREATE INDEX idx_crossroads_project_status
    ON crossroads(project_ref, status, updated_at_ms, id);
CREATE INDEX idx_crossroads_status
    ON crossroads(status, updated_at_ms, id);

CREATE TABLE decision_records (
    id TEXT PRIMARY KEY,
    crossroad_id TEXT,
    request_ref TEXT,
    task_ref TEXT,
    project_ref TEXT,
    repository TEXT,
    source_session_id TEXT,
    source_turn_id TEXT,
    status TEXT NOT NULL,
    actor TEXT NOT NULL,
    approval_state TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE INDEX idx_decisions_crossroad
    ON decision_records(crossroad_id, recorded_at_ms, id);
CREATE INDEX idx_decisions_session
    ON decision_records(source_session_id, recorded_at_ms, id);
CREATE INDEX idx_decisions_project_repository
    ON decision_records(project_ref, repository, recorded_at_ms, id);
CREATE INDEX idx_decisions_status_actor
    ON decision_records(status, actor, updated_at_ms, id);

CREATE TABLE decision_warrants (
    id TEXT PRIMARY KEY,
    decision_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE INDEX idx_decision_warrants_decision
    ON decision_warrants(decision_id, created_at_ms, id);

CREATE TABLE decision_change_sets (
    id TEXT PRIMARY KEY,
    decision_id TEXT,
    session_id TEXT,
    scratchpad_id TEXT,
    commit_sha TEXT,
    git_intent_note_ref TEXT,
    pull_request TEXT,
    issue TEXT,
    created_at_ms INTEGER NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE INDEX idx_change_sets_decision
    ON decision_change_sets(decision_id, created_at_ms, id);
CREATE INDEX idx_change_sets_commit
    ON decision_change_sets(commit_sha, created_at_ms, id);
CREATE INDEX idx_change_sets_pull_request
    ON decision_change_sets(pull_request, created_at_ms, id);
CREATE INDEX idx_change_sets_session
    ON decision_change_sets(session_id, created_at_ms, id);

CREATE TABLE provenance_relationships (
    id TEXT PRIMARY KEY,
    from_type TEXT NOT NULL,
    from_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    to_type TEXT NOT NULL,
    to_id TEXT NOT NULL,
    evidence TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    UNIQUE(from_type, from_id, relation, to_type, to_id)
);

CREATE INDEX idx_provenance_relationships_from
    ON provenance_relationships(from_type, from_id, relation, created_at_ms, id);
CREATE INDEX idx_provenance_relationships_to
    ON provenance_relationships(to_type, to_id, relation, created_at_ms, id);

CREATE TABLE provenance_notifications (
    id TEXT PRIMARY KEY,
    category TEXT NOT NULL,
    preference_boundary_id TEXT,
    crossroad_id TEXT,
    decision_id TEXT,
    authority_required TEXT,
    created_at_ms INTEGER NOT NULL,
    payload_json TEXT NOT NULL
);

CREATE INDEX idx_provenance_notifications_category
    ON provenance_notifications(category, created_at_ms, id);
CREATE INDEX idx_provenance_notifications_boundary
    ON provenance_notifications(preference_boundary_id, created_at_ms, id);
CREATE INDEX idx_provenance_notifications_decision
    ON provenance_notifications(decision_id, created_at_ms, id);

CREATE TABLE provenance_projection_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT INTO provenance_projection_meta(key, value)
VALUES ('schema_version', '1')
ON CONFLICT(key) DO NOTHING;
