# Decision provenance projection

Codex stores the canonical decision-provenance event log and materialized
records in the existing state SQLite database. The local projection is an
optional, read-only consumer view for Inbound and future adapters. It is not a
second writer or a transcript.

## Location and version

For a state home `STATE_HOME`, Codex writes:

```text
STATE_HOME/decision-provenance/projection-v1.json
```

The top-level object has `schema_version: 1`, `read_only: true`, and these
collections:

```json
{
  "schema_version": 1,
  "generated_at": "RFC3339 timestamp",
  "read_only": true,
  "source_event_id": "latest canonical event ID or null",
  "source_event_recorded_at": "latest canonical event timestamp or null",
  "truncated": false,
  "decisions": [],
  "crossroads": [],
  "preference_boundaries": [],
  "warrants": [],
  "change_sets": [],
  "relationships": [],
  "notifications": [],
  "indexes": {
    "decision_id": {},
    "crossroad_id": {},
    "preference_boundary_id": {},
    "session_id": {},
    "repository": {},
    "project": {},
    "commit_sha": {},
    "pull_request": {},
    "timestamp": {},
    "status": {},
    "actor": {},
    "scope": {}
  }
}
```

Collection entries use the versioned Rust domain model. Relationship
`evidence` distinguishes `explicit`, `inferred`, and `considered`; consumers
must not turn an inferred or considered link into stronger causality.

## Consistency and privacy

Events are appended transactionally before materialized records are updated.
The projection is generated from one SQLite snapshot, written to a unique
temporary file, synced, and replaced into place only after that snapshot is
committed. `source_event_id` and `source_event_recorded_at` form a canonical
watermark; a consumer can request a refresh when the watermark is behind the
event log. `truncated: true` means one or more collections reached the bounded
projection cap and the consumer must not treat the snapshot as complete. A
missing, malformed, or unknown-version projection is rebuilt from SQLite before
it is returned by the state API. Inbound should treat the file as a cache and
never edit it.

Decision records store summaries and source references, not rollout contents,
tool arguments, scratchpad task copies, memory text, or hidden reasoning.
Sensitive fields and secret-like values are redacted before canonical event
payloads are stored. A private local record may still reference a public
commit, pull request, session, or scratchpad ID without publishing its private
rationale.

The `preference_boundaries` collection is the canonical typed record for a
boundary's lifecycle and its provenance history. It is not a second generic
preference database: user-preferences memory remains authoritative for generic
reusable guidance and its event stream, while `related_memory_record_id` points
to the memory observation that led to this boundary without copying that event
stream. Boundary lifecycle commands update only the provenance boundary record;
subsequent memory observations can create an explicit replacement, but an
inferred observation cannot rewrite a confirmed user boundary. Inbound must
treat these boundary entries as read-only.

The event schema has its own `schema_version` and the SQLite migration is
append-only. Future projection versions should use a new filename or explicit
reader negotiation rather than changing the meaning of `projection-v1.json`.
