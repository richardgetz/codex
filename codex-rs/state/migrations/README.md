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
