# Fork differences

This fork tracks [`openai/codex`](https://github.com/openai/codex) and keeps a
small set of intentional differences on top.

Use this page as the index for anything that exists only in `@rickgetz/codex`
or behaves differently from upstream.

## Current differences

### Distribution

- npm package: `@rickgetz/codex`
- Primary install command: `npm install -g @rickgetz/codex`
- Installed executable: `codex-rick`
- The upstream `@openai/codex` package can remain installed as `codex` for
  fallback use.
- GitHub releases come from this fork, not the upstream OpenAI repository.

### Release lane

- Merges or pushes to `stable` automatically produce fork releases.
- Fork versions use the format `<upstream version>-rick.<counter>`.
- Git tags use the format `rick-v<upstream version>-rick.<counter>`.
- The automated release lane currently publishes Apple Silicon macOS binaries only.

See [Fork npm releases](./fork-release.md) for the release workflow details.

### Feature toggles

- This fork carries `enable_mcp_approvals` as a Rick-owned feature.
- Persist it in config with:
  - `codex features enable enable_mcp_approvals`
  - `codex features disable enable_mcp_approvals`
- Override it for one run with:
  - `codex --enable enable_mcp_approvals`
  - `codex --disable enable_mcp_approvals`
- `codex features list` marks Rick-owned features with `(rick)`.

### Orchestrator mode defaults

- Orchestrator mode can be selected at launch with `--collab orchestrator`.
- One-letter, case-insensitive collaboration-mode shorthands are supported; for
  example `--collab o` starts Orchestrator mode.
- Orchestrator mode uses fork-specific model defaults when the config does not
  specify `[thread_control.orchestrator]`.
- Child agents launched by Orchestrator mode are restricted by
  `[orchestrator].allowed_spawn_modes`; the fork default is `["default"]` to
  avoid recursive orchestrator loops.
- Orchestrator active-agent check-ins are patient supervision wake-ups. They are
  intended to clarify, redirect, unblock, or keep waiting; they are not urgency
  prompts to tell workers to move faster.
- Spawned child agents send a waking completion notification back to their
  direct parent when they complete or abort a turn. In Orchestrator mode that
  notification carries an explicit user-delivery obligation so a child answer is
  relayed instead of being treated as inert watch state. Memory extraction and
  consolidation agents are excluded from these hooks to avoid feedback loops.

### Primary contact channel

- Orchestrator mode can start a configured communication MCP at session boot
  mechanically, without injecting a startup prompt or spending a model turn:

  ```toml
  [orchestrator]
  primary_contact = { enabled = true, mcp = "imessage" }
  ```

- `--primary-contact <mcp>` overrides the configured primary contact for one
  launch; `--primary-contact off` disables it for that launch.
- The primary-contact poller checks for new user messages without calling the
  model unless a new message is found. When the channel is active, the TUI shows
  a lightweight monitoring notice:

  ```toml
  [orchestrator]
  primary_contact = { enabled = true, mcp = "imessage", check_messages_every_seconds = 900 }
  ```

- The default interval can be overridden by an optional local-time schedule:

  ```toml
  [orchestrator.primary_contact]
  enabled = true
  mcp = "imessage"
  check_messages_every_seconds = 900

  [[orchestrator.primary_contact.schedule]]
  days = ["weekdays"]
  start = "07:00"
  end = "22:00"
  check_messages_every_seconds = 300

  [[orchestrator.primary_contact.schedule]]
  start = "22:00"
  end = "07:00"
  check_messages_every_seconds = 1800
  ```

- Schedule entries use local `HH:MM` time. `days` may be omitted for every day,
  or set to day names like `"mon"`/`"monday"`, `"weekdays"`, or `"weekends"`.
  Overnight windows are supported.
- `check_messages_every_seconds = 0` disables the harness-level poller.
- When the poller is armed and no model turn is active, the terminal title uses
  a static waiting braille marker so a headless or background window still looks
  alive without spending model calls.

### Orchestrator memory

- Orchestrator memory defaults to enabled for this fork:

  ```toml
  [orchestrator_memory]
  enabled = true
  scope = "orchestrator"
  model_on_heuristic_miss = false
  model_consolidation = false
  ```

- The memory classifier is broader than task reminders: it should retain durable
  user preferences, working style, follow-up intent, operator playbooks, and
  other continuity notes when the user signals they matter later.
- `/orchestrator-memory-forget <needle>` removes matching orchestrator-memory
  entries without touching mainline memory stores.
- Explicit forget requests such as `forget this: ...` are treated as memory
  removal requests.
- To avoid silent background model spend, heuristic misses do not invoke a
  classifier model by default, and summary/profile consolidation uses the
  mechanical renderer by default. Set `model_on_heuristic_miss = true` or
  `model_consolidation = true` to restore those model-assisted paths.
- Memory events are mirrored into bucket-specific files under
  `<codex_home>/orchestrator_memory/buckets/` for easier inspection while
  preserving `preferences.jsonl` as the compatibility event log.
- Legacy memory events that predate bucketed schemas are migrated on the next
  read or consolidation, with a `preferences.jsonl.pre-bucket-migration` backup.

### Built-in scratchpad

- Default, Continuous, and Orchestrator modes treat scratchpad as a first-class
  recovery ledger. Plan mode does not use built-in scratchpad by default.
- The fork has a canonical built-in `scratchpad` tool namespace. When a
  configured scratchpad MCP exposes the same namespace, the built-in namespace
  stays model-visible and its handlers take precedence.
- Agents receive mode-scoped developer guidance explaining when and how to use
  the built-in scratchpad. If built-in scratchpad is disabled for a mode, the
  tool namespace and guidance are both omitted for that mode.
- Built-in scratchpads are JSON-backed under `<codex_home>/scratchpad/entries`
  unless a tool call provides `state_home`.
- `<codex_home>/scratchpad` is created and added to workspace-write writable
  roots automatically, alongside memory and supervision roots.
- `open_scratchpad` defaults `scratchpad_id` to the current Codex
  thread/session id when no explicit id is provided.
- `resume_scratchpad` strictly reopens an existing scratchpad by id without
  creating a replacement; archived pads require `include_archived = true`.
- Built-in scratchpad supports active and archived lookup, archive/unarchive,
  next-step and pending-wait updates, action-policy checks, and wait check-ins.
- After a live context compaction item is observed in a scratchpad-enabled mode,
  the fork can mechanically read the built-in scratchpad for the active thread
  id and inject the recovered state into the next model turn. Replayed history
  does not fire this hook.
- Built-in scratchpad and compaction recovery are controlled globally and per
  mode with:

  ```toml
  [scratchpad]
  enabled = true
  recover_after_compaction = true

  [scratchpad.modes.plan]
  enabled = false
  recover_after_compaction = false

  [scratchpad.modes.orchestrator]
  enabled = true
  recover_after_compaction = true
  ```

- The legacy `[orchestrator].recover_scratchpad_after_compaction` key remains
  supported as an Orchestrator-only compatibility alias.

### Built-in schedule

- The fork has a canonical built-in `schedule` tool namespace for durable
  reminders, recurring routines, and conditional future checks.
- Orchestrator mode exposes it by default. Default and Continuous modes can opt
  in. Plan mode is disabled by default.
- Scheduled triggers are JSON-backed under `<codex_home>/schedule/triggers`
  unless a tool call provides `state_home`.
- `<codex_home>/schedule` is created and added to workspace-write writable roots
  automatically.
- Agents receive mode-scoped developer guidance explaining when to use schedule,
  when to prefer scratchpad pending waits instead, and how to link triggers to
  built-in scratchpad ids or orchestrator memory context.
- Built-in schedule exposure is controlled globally and per mode with:

  ```toml
  [schedule]
  enabled = false

  [schedule.modes.orchestrator]
  enabled = true

  [schedule.modes.default]
  enabled = true
  ```

- The namespace supports create/get/list/list-due/update/close/reopen/delete,
  `mark_scheduled_trigger_fired`, and schema discovery.
- Recurrence metadata is preserved as structured JSON. `interval_seconds` is
  mechanically advanced by `mark_scheduled_trigger_fired`; richer schedules can
  store their source text/timezone/day combination for future runners or agents.

### Account aliases

- `--account <alias>` starts a session using a managed account alias.
- `/account <alias>` switches the current session to a managed alias.
- `/account default` returns the session to the original root auth store.
- `/status` displays managed aliases as `<alias> - <email> (<account type>)`
  when an alias is active.
- Account alias selection is session-scoped so multiple Codex sessions can spend
  against different accounts concurrently.

### MCP visibility and inventory

- `/mcp` includes mode-aware visibility in this fork so Orchestrator mode can
  distinguish configured/available MCPs from MCPs hidden by the current mode.
- The prompt includes current MCP availability context so agents can answer
  questions about which MCPs are usable in the exact running harness instead of
  relying on stale docs.

### Fork-aware help

- The fork exposes repo-local fork help context so agents can answer questions
  like "what's available in Rick's fork?" or "what's new in this fork version?"
  from checked-in fork documentation rather than from upstream OpenAI docs.
- Keep this page updated whenever a fork-only behavior changes user-visible
  commands, flags, config, defaults, or recovery behavior.

## Fork-only feature labeling

If this fork adds an experimental feature that surfaces its own help text in the
UI or app-server metadata, that help text must be labeled with a `(rick)` prefix.

The enforcement point for that lives in
`codex-rs/features/src/lib.rs`:

- experimental features declare an explicit `owner`
- `FeatureOwner::Rick` automatically prefixes user-facing descriptions and announcements with `(rick)`

That means new fork-only experimental features should:

1. set `owner: FeatureOwner::Rick`
2. add or update an entry on this page if the feature changes fork behavior

Do not add entries here for intended differences that are not actually active in
this fork yet.
