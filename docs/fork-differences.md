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
- Fork-only state DB migrations must keep already-shipped numeric versions
  stable. New fork migrations should use the next unused number and include
  `rick` in the filename, for example `0031_rick_short_feature_name.sql`.
  During upstream refreshes, if an upstream migration number collides with a
  shipped fork migration, preserve the fork migration filename/checksum and move
  the upstream migration to the next unused version. See
  [`codex-rs/state/migrations/README.md`](../codex-rs/state/migrations/README.md).

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
- Standalone Continuous collaboration mode is removed. Use `/continuous` inside
  a normal session to enable or disable continuous execution for that thread.
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

### Orchestrator session overwatch

- Orchestrator mode exposes a built-in `session_overwatch` namespace for
  supervising Codex sessions that were not necessarily launched by the current
  orchestrator thread.
- The namespace includes `list_sessions`, `watch_session`, `unwatch_session`,
  and `message_session`.
- Watches are recorded in the local state database as Router thread-control
  targets, so already-started sessions can emit durable completion signals back
  to the watching orchestrator when a turn completes or aborts.
- Watched sessions also appear in the orchestrator supervision summary so idle
  check-ins can stay model-light unless a watched session changes state.
- `message_session` delivers immediately to sessions that are live in the same
  Codex process. If the target session belongs to another CLI process, the
  message is queued in the durable session inbox and the target CLI injects it
  mechanically as normal user input when its inbox poller sees it.
- Cross-process delivery is not a hard interrupt while the target process is
  inside a model request. It is a model-less durable inbox handoff that lets
  separate running CLIs communicate once the target session drains pending
  input.

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

  [orchestrator_memory.cleanup]
  enabled = true
  schedule = "03:30"
  run_missed_on_startup = true
  dedupe_raw_events = true
  deep_consolidation = true
  model_consolidation = true
  retain_forget_events_days = 30
  ```

- The memory classifier is broader than task reminders: it should retain durable
  user preferences, working style, follow-up intent, operator playbooks, and
  other continuity notes when the user signals they matter later.
- `/orchestrator-memory-forget <needle>` removes matching orchestrator-memory
  entries without touching mainline memory stores.
- `/orchestrator-memory-consolidate` triggers the configured orchestrator-memory
  cleanup/consolidation path immediately, which is useful for testing cleanup
  behavior without changing the configured schedule.
- Explicit forget requests such as `forget this: ...` are treated as memory
  removal requests.
- To avoid silent background model spend, heuristic misses do not invoke a
  classifier model by default, and summary/profile consolidation uses the
  mechanical renderer by default. Set `model_on_heuristic_miss = true` or
  `model_consolidation = true` to restore those model-assisted paths.
- Memory events are mirrored into bucket-specific files under
  `<codex_home>/orchestrator_memory/buckets/` for easier inspection while
  preserving `preferences.jsonl` as the canonical event log.
- Scheduled cleanup runs at most once per day by local time, executes on the next
  startup if the scheduled time was missed, compacts duplicate raw events,
  retains recent forget tombstones, and resyncs bucket mirrors. By default it
  also runs a `Memory [memory builder]` sub-agent to merge semantic
  near-duplicates before regenerating summary/profile artifacts; set
  `cleanup.model_consolidation = false` for mechanical-only cleanup.
- Legacy memory events that predate bucketed schemas are migrated on the next
  read or consolidation, with a `preferences.jsonl.pre-bucket-migration` backup.

### Built-in scratchpad

- Default and Orchestrator modes treat scratchpad as a first-class
  recovery ledger. Plan mode does not use built-in scratchpad by default.
- `/continuous` toggles a scratchpad-backed continuous run policy for the
  current thread. When `run_policy.continuous.enabled` is true and the
  scratchpad still has `next_steps` or `pending_waits`, Codex loops back to the
  scratchpad instead of finalizing.
- Scratchpads include `communication_policy` for durable communication
  preferences; channel failure alone is not treated as permission to stop or
  fall back to a final response.
- The fork has a canonical built-in `scratchpad` tool namespace. When a
  configured scratchpad MCP exposes the same namespace, the built-in namespace
  stays model-visible and its handlers take precedence.
- Agents receive mode-scoped developer guidance explaining when and how to use
  the built-in scratchpad. If built-in scratchpad is disabled for a mode, the
  tool namespace and guidance are both omitted for that mode.
- Built-in scratchpads are JSON-backed under `<codex_home>/scratchpad/entries`
  unless a tool call provides `state_home`.
- A generated `<codex_home>/scratchpad/index.json` manifest lists scratchpads by
  id, objective, status, session key, creation time, update time, and archive
  time so recent work can be found without manually scanning every entry file.
- `<codex_home>/scratchpad` is created and added to workspace-write writable
  roots automatically, alongside memory and supervision roots.
- Built-in scratchpad tools are bound to the current Codex thread/session id.
  `open_scratchpad` defaults `scratchpad_id` to that id when omitted, and
  model-visible tools reject custom or other-thread scratchpad ids.
- `resume_scratchpad` strictly reopens the current thread scratchpad without
  creating a replacement. Archived pads remain readable and editable by their
  owning thread until lifecycle deletion.
- Built-in scratchpad supports active and archived lookup, archive/unarchive,
  next-step and pending-wait updates, action-policy checks, and wait check-ins.
- Scratchpads can record measurable outcomes with `record_outcome`, using
  portable datapoints scoped to a service, endpoint, function, feature, or other
  surface. Outcome entries can include metric/unit, baseline/current/delta,
  summary, tradeoffs, artifacts, commit, and PR provenance.
- Outcome recording is opt-in and defaults off. Set
  `[scratchpad].outcomes_enabled = true` or run `/outcomes on` to allow agents
  to record new outcome datapoints; `/outcomes off` disables it persistently.
- `/outcomes` renders the current session scratchpad outcomes as a markdown
  postmortem summary. The same data is exportable through `export_outcomes` as
  JSON plus markdown for sharing or later visualization.
- Legacy `continuous` collaboration-mode values in old config or rollout
  payloads deserialize as `default` for compatibility only; they do not enable
  continuous policy. Use `/continuous on` for the scratchpad-backed runtime
  behavior.
- Scratchpads can record delegated work lineage with `record_delegation`,
  including the subagent id/label, parent item references, child scratchpad id,
  status, notes, and artifacts so parent-child work ownership survives restart.
- `/scratchpad` renders the current session's built-in scratchpad on demand,
  including current objective, status, completed work, next steps, and waits.
- `/scratchpad-absorb <scratchpad_id>` copies another scratchpad into the
  current thread scratchpad as contextual history without changing source
  ownership or importing live control policy. It includes pending waits by
  default; use `--exclude-pending` to omit them.
- `/scratchpad-unarchive` clears the archived marker from the current thread
  scratchpad so it is no longer eligible for archived-pad cleanup.
- Live TUI scratchpad update cards are compact by default: completed work shows
  only the newest item, while next steps and waits each show up to five items.
  `/scratchpad` remains verbose and renders the full scratchpad regardless of
  live-card limits.
- When a session resumes and the thread-id scratchpad already exists with
  uncompleted work (`next_steps` or `pending_waits`), Codex injects the
  scratchpad id and compact scratchpad state into hidden developer context so
  the agent can continue the same recovery ledger without searching. Completed
  and archived scratchpads are skipped.
- Scratchpad lifecycle cleanup runs mechanically during config load. By
  default, non-archived pads are archived after 30 days without updates, and
  archived pads are deleted after 90 days in the archive.
- After context compaction, actionable built-in scratchpad state is looped back
  through hidden developer context, using the same model-visible hidden-context
  path as other post-compaction recovery state rather than a synthetic user
  turn.
- Built-in scratchpad availability is controlled globally and per mode with:

  ```toml
  [scratchpad]
  enabled = true
  recover_after_compaction = true
  auto_archive_after_days = 30
  delete_archived_after_days = 90

  [scratchpad.view]
  enabled = true
  show_id = true
  completed_items = 1
  next_steps = 5
  pending_waits = 5

  [scratchpad.modes.plan]
  enabled = false
  recover_after_compaction = false

  [scratchpad.modes.orchestrator]
  enabled = true
  recover_after_compaction = true
  ```

- The legacy `[orchestrator].recover_scratchpad_after_compaction` key remains
  supported as an Orchestrator-only compatibility alias.

### Fast resume

- Session resume remains compatible with upstream/mainline rollout JSONL files;
  the fork does not require a sidecar cache or migration.
- By default, resume reverse-scans the existing rollout from the end, finds the
  newest compaction item with `replacement_history`, and reconstructs from that
  compacted baseline plus the surviving tail instead of parsing the whole file.
- If no safe replacement compaction exists, Codex falls back to full replay.
- The app-server thread response lazily hydrates visible turns by default so
  very large sessions do not need to render their entire historical UI payload
  before becoming usable.
- Config:

  ```toml
  [resume]
  strategy = "latest_compaction" # or "full"
  visible_turn_limit = 80
  lazy_hydrate_history = true
  load_timeout_seconds = 60
  inject_scratchpad = true
  ```

### Built-in schedule

- The fork has a canonical built-in `schedule` tool namespace for durable
  reminders, recurring routines, and conditional future checks.
- Orchestrator mode exposes it by default. Default mode can opt in. Plan mode
  is disabled by default.
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
- If Codex sees `MCP startup cancelled` while resolving a configured MCP, it
  retries startup in a bounded way instead of leaving the cancelled startup
  memoized for the rest of the session.
- If a model calls a previously seen MCP tool through an unavailable placeholder
  name such as `mcp__aws_auth_guard__auth_guard_status`, Codex maps that plain
  placeholder back to the configured MCP server, forces a server tool-list/start
  path, and resolves the real MCP tool when the daemon is available.
- That recovery is intentionally bounded to configured MCP servers in the
  current session. It does not create arbitrary MCP servers from unknown tool
  names.

### Agent browser

- `[features].agent_browser = true` exposes the built-in `agent_browser`
  namespace when the session has a local execution environment.
- The namespace provides CDP-backed browser control over multiple launch
  backends: `auto`, `obscura`, and `chromium`. `auto` prefers the Rust-native
  Obscura backend for headless agent sessions when an `obscura` binary is
  available, and falls back to Chromium for broader visual-review support.
- Tools include open/attach, navigate, snapshot, screenshot, click, type, press,
  scroll, selection-overview, highlight, live-session sharing, and benchmark.
  `agent_browser.share` writes a local share token that another agent can pass
  to `agent_browser.open` to attach to the same live page instead of relaunching
  and rebrowsing. Shares default to `read_only`; `read_write` is available for
  deliberate handoffs. Share tokens are leased by default for one hour, can be
  shortened or extended up to twelve hours, and expired tokens are cleaned up
  when browser shares are read or created.
- Benchmarks can target the default local page or an explicit URL, and report
  screenshot PNG/base64 sizes alongside latency so transport-size tradeoffs stay
  visible. Obscura currently covers CDP navigation/evaluation/input/snapshot
  flows; screenshots use a lightweight DOM snapshot renderer until native
  compositor screenshots are added. Obscura `mode = "headful"` opens a small
  local mirror shell driven by the same CDP snapshot path so the Rust-native
  backend is not limited to invisible sessions.
- Set `CODEX_AGENT_BROWSER_OBSCURA_BINARY` to point at a custom Obscura binary,
  bundle `obscura` next to the Codex executable, or on macOS bundle it in
  `Codex.app/Contents/Resources/obscura`. This keeps the Codex Rust crates lean
  while still letting app distributions ship Obscura as a first-party resource.
  Use `backend = "chromium"` to force the Chromium launch path.
- When attaching to an existing CDP endpoint, Codex creates an `about:blank`
  page target if `/json/list` has no debuggable page yet, then closes only that
  owned target when the session closes.
- The browser compatibility profile is enabled by default through the
  `stealth` option name for API compatibility. It is intended for legitimate UI
  testing and review automation, not for avoiding authentication, payment, rate,
  or usage policy requirements. It isolates launch state in a temporary profile,
  applies review-focused Chromium flags, reduces obvious webdriver-only
  differences, and normalizes the default headless user agent.
- The collaborative overlay is injected only when
  `agent_browser.selection_overview` or `agent_browser.highlight` is requested,
  keeping ordinary browsing paths lighter and less page-mutating. Highlights can
  mark a snapshot element ref or viewport rect and are returned in the overlay
  overview payload.

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
