# State Migration Policy For This Fork

State migrations are versioned by the numeric prefix in the filename. Once a
migration ships on `stable`, do not rename it, renumber it, or change its SQL.
Existing user databases validate known migration checksums on startup.

Fork-only migrations should use the next unused version and include `rick` in
the filename so future upstream refreshes can identify the source quickly:

```text
0031_rick_short_feature_name.sql
```

When an upstream release adds a migration number that collides with an already
shipped fork migration, keep the shipped fork migration exactly as-is and append
the upstream migration at the next unused version:

```text
0028_rick_existing_fork_feature.sql   # already shipped, do not change
0029_upstream_original_feature.sql    # upstream migration moved forward
```

The SQL should still be reviewed for object-name conflicts, but a version
collision alone does not imply the schema changes conflict.

The `rust-v0.146.0` refresh keeps the already-shipped fork migrations at
`0043_upstream_threads_name.sql` and `0044_upstream_drop_agent_jobs.sql`. The
incoming upstream migrations are appended as:

```text
0045_upstream_threads_is_pinned.sql
0046_upstream_external_agent_config_imports_provider_id.sql
```

The `rust-v0.147.0` refresh keeps those already-shipped migrations unchanged.
Its incoming thread-section migrations are appended as:

```text
0047_upstream_threads_section.sql
0048_upstream_threads_section_order.sql
```

The `rust-v0.148.0` refresh kept those already-shipped migrations unchanged.
Its incoming rollout migration and thread-section appearance migration were
appended as:

```text
0049_upstream_rollout_migration_state.sql
0050_upstream_thread_section_appearance.sql
```

The `rust-v0.149.0` refresh keeps those already-shipped migrations unchanged.
Its incoming project and thread-section index migrations collide with those
numeric versions, so they are appended at the next unused versions:

```text
0051_upstream_projects.sql
0052_upstream_threads_section_empty_preview_indexes.sql
```

The `rust-v0.150.1` refresh keeps those already-shipped migrations unchanged.
Its incoming thread-artifacts migration collides with the existing `0051`
through `0052` sequence, so it is appended as:

```text
0053_upstream_thread_artifacts.sql
```

The decision-provenance layer is appended as `0054_rick_decision_provenance.sql`.
Its event table is canonical and append-only; the other provenance tables are
materialized query indexes. The versioned Inbound projection format is
documented in `state/src/decision_provenance/PROJECTION.md`.
