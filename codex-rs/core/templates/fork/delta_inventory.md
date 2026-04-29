# Fork Delta Inventory

This file tracks fork-only changes that ship with this build. Keep it updated as
the fork evolves, and use it as a merge-awareness checklist whenever upstream
stable/mainline is pulled in.

## Introduced In 0.124.0-rick.2 (Recent)

- Account alias switching:
  - CLI: `codex --account <alias>`
  - In-session: `/account <alias>` and `/account default`
  - Behavior: alias auth resolves through `~/.codex/accounts/<alias>`, while
    the root auth store remains the default for compatibility with mainline
    Codex.
  - Storage policy: root/default auth stays file-compatible for mainline, while
    managed aliases default to keychain-first `auto` storage with file
    fallback when keychain is unavailable.
- Orchestrator startup mode selection:
  - CLI: `codex --collab <mode>`
  - Supports non-case-sensitive values and one-letter shorthands such as `o`
    for `orchestrator`.
- Orchestrator defaults:
  - Default model/reasoning when no override is set:
    `gpt-5.3-codex-spark` + `low`
  - Fallback on ChatGPT-account unsupported-model errors:
    `gpt-5.5` + `low`
  - Coding-task subagents should prefer `gpt-5.5`, selecting reasoning effort
    by task difficulty: `low` for exploration/mechanical work, `medium` for
    clear implementation and straightforward fixes, `high` for complex or
    unclear work, and `xhigh` only for extreme or explicitly requested cases
    after checking with the user unless already instructed.
- Orchestrator memory defaults:
  - `[orchestrator_memory]`
  - `enabled = true`
  - `scope = "orchestrator"`
- Orchestrator memory maintenance:
  - Slash command: `/orchestrator-memory-forget <needle>`
  - Slash command: `/orchestrator-memory-consolidate`
  - Bucket-specific mirror files live under
    `<codex_home>/orchestrator_memory/buckets/`.
  - Scheduled cleanup runs daily by local `HH:MM` schedule, defaults to `03:30`,
    compacts duplicate raw events in `preferences.jsonl`, keeps recent forget
    tombstones, resyncs bucket files, and defaults to a `Memory [memory builder]`
    semantic merge pass before regenerating summary/profile artifacts.
  - Legacy unbucketed memory events are migrated on next read/consolidation with
    a `preferences.jsonl.pre-bucket-migration` backup.
- Mode-scoped enablement filters:
  - `[enablement.modes.<mode>]`
  - Supports `skills`, `mcps`, and `plugins`
  - Each filter uses `{ mode = "include"|"exclude", items = [...] }`
  - `items = ["*"]` is supported
- Orchestrator spawn safety:
  - `[orchestrator].allowed_spawn_modes`
  - Default child mode allow-list is `["default"]`
- Orchestrator inline MCP usage:
  - Explicitly enabled Orchestrator MCPs may run in the parent Orchestrator
    thread for communication/state work instead of forcing a child worker.
- MCP visibility recovery:
  - Cancelled MCP startups are retried in a bounded way instead of memoizing the
    cancelled startup for the rest of the session.
  - Plain unavailable MCP placeholder calls such as
    `mcp__aws_auth_guard__auth_guard_status` are mapped back to configured MCP
    servers, forcing a server tool-list/start path and resolving the real MCP
    tool when the daemon is available.
- Orchestrator session overwatch:
  - Built-in namespace: `session_overwatch`
  - Tools: `list_sessions`, `watch_session`, `unwatch_session`,
    `message_session`
  - Watches are persisted as Router thread-control targets so already-started
    sessions can emit durable completion signals back to the watching
    orchestrator when a turn completes or aborts.
  - `message_session` delivers immediately to sessions live in the same Codex
    process and queues durable inbox messages for sessions owned by another
    local CLI process.
  - Each running CLI session polls its own durable inbox without model spend and
    injects queued messages as normal user input when found.
- Orchestrator primary contact channel:
  - Config: `[orchestrator].primary_contact`
  - Startup override: `--primary-contact <mcp>` or `--primary-contact off`
  - Harness-only polling uses `check_messages_every_seconds`, default `900`,
    and only calls the model when a new user message is found.
  - Armed idle polling uses a static terminal-title waiting marker so the
    window still appears alive without model calls.
- Built-in scratchpad:
  - Namespace: `scratchpad`
  - Default, Continuous, and Orchestrator modes expose it by default; Plan mode
    does not.
  - The built-in namespace is canonical; if a configured scratchpad MCP exposes
    the same namespace, the built-in spec remains model-visible and built-in
    handlers take precedence.
  - Agents receive built-in scratchpad developer guidance in enabled modes.
  - Stores JSON scratchpads under `<codex_home>/scratchpad/entries` unless a
    tool call provides `state_home`.
  - Maintains generated `<codex_home>/scratchpad/index.json` metadata for
    recent-work lookup without changing canonical per-scratchpad JSON storage.
  - `<codex_home>/scratchpad` is created and added to workspace-write writable
    roots automatically.
  - Config: `[scratchpad]` with mode overrides under
    `[scratchpad.modes.<mode>]`
  - Keys: `enabled`, `recover_after_compaction`,
    `auto_archive_after_days`, `delete_archived_after_days`
  - `open_scratchpad` defaults `scratchpad_id` to the current thread/session id.
  - `resume_scratchpad` strictly reopens an existing scratchpad by id without
    creating a replacement; archived pads require `include_archived = true`.
  - Slash command: `/scratchpad` renders the current session scratchpad on
    demand using the same status-card UI as live scratchpad updates.
  - Resume injects the active thread scratchpad id and compact scratchpad state
    into hidden developer context when the thread-id scratchpad exists with
    uncompleted work (`next_steps` or `pending_waits`).
  - Supports active/archived lookup, archive/unarchive, next-step and
    pending-wait updates, action-policy checks, and wait check-ins.
  - Lifecycle cleanup runs during config load. Defaults: archive non-archived
    pads after 30 days without updates; delete archived pads after 90 days in
    archive. Set either day value to `0` to disable that phase.
- Post-compaction recovery:
  - Config: `[scratchpad].recover_after_compaction` and
    `[scratchpad.modes.<mode>].recover_after_compaction`
  - Default: `true`
  - In scratchpad-enabled modes, actionable built-in scratchpad state is looped
    back through hidden developer context after compaction. Completed or
    archived scratchpads are not looped back, and the TUI does not synthesize a
    user turn for recovery state.
  - Legacy `[orchestrator].recover_scratchpad_after_compaction` remains
    supported as an Orchestrator-only compatibility alias.
- Fast resume:
  - Config: `[resume]`
  - Defaults: `strategy = "latest_compaction"`, `visible_turn_limit = 80`,
    `lazy_hydrate_history = true`, `load_timeout_seconds = 60`,
    `inject_scratchpad = true`
  - Uses the existing rollout JSONL format directly; no required sidecar file.
  - Reverse-scans from the end to the newest replacement-history compaction and
    reconstructs from that checkpoint plus the surviving tail, falling back to
    full replay when no safe checkpoint exists.
- Fork docs links:
  - Public README docs links point at the fork `stable` branch because npm
    renders package README links relative to `codex-cli`.

## Earlier Fork Deltas

- Orchestrator is coordination-only:
  - execution work should go to child agents
  - communication with the user stays in the orchestrator thread
- Memory helpers have human-readable names:
  - `Memory [extractor]`
  - `Memory [memory builder]`
- Orchestrator supervision avoids wasteful idle model polling:
  - `[orchestrator].active_agent_checkin_seconds`
- Collaboration-mode skill filtering exists and now rolls up under the unified
  enablement model.

## Merge Checklist

- Verify `codex --collab ...` still applies the intended collaboration mode and
  Orchestrator thread-control defaults.
- Verify `codex --account ...` and `/account ...` still switch auth stores
  without breaking the default root auth location.
- Verify `/orchestrator-memory-forget <needle>` still prunes and reconsolidates
  orchestrator memory, including bucket mirror files.
- Verify `/orchestrator-memory-consolidate` still triggers a manual
  orchestrator-memory cleanup pass.
- Verify `[enablement.modes.<mode>]` still filters `skills`, `mcps`, and
  `plugins` correctly.
- Verify Orchestrator child spawns still respect
  `[orchestrator].allowed_spawn_modes`.
- Verify explicitly enabled Orchestrator MCPs remain callable inline for
  communication/state workflows.
- Verify cancelled MCP startup can retry, and a plain unavailable MCP
  placeholder call can recover the configured server namespace instead of
  permanently reporting the tool unavailable.
- Verify `session_overwatch` lists sessions, can watch/unwatch existing thread
  ids, records watched sessions in the supervision summary, and can queue
  cross-process `message_session` input that the target CLI later injects.
- Verify configured primary contact polling starts in Orchestrator mode and does
  not wake the model for empty status responses, while the terminal title shows
  the waiting marker when idle.
- Verify built-in `scratchpad` remains available in Default, Continuous, and
  Orchestrator modes, omitted from Plan mode by default, and
  `open_scratchpad` uses the thread id when no id is provided.
- Verify built-in `resume_scratchpad` refuses to create a new scratchpad and
  requires explicit `include_archived = true` for archived pads.
- Verify configured scratchpad MCPs do not shadow the built-in scratchpad
  namespace.
- Verify post-compaction built-in scratchpad loopback is hidden from the TUI and
  only injects actionable scratchpads with `next_steps` or `pending_waits`, not
  completed or archived scratchpads.
- Verify memory helper naming still shows `Memory [extractor]` and
  `Memory [memory builder]`.
