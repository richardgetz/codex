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

### macOS GPU compute in Seatbelt sandboxes

- The fork's macOS Seatbelt base policy intentionally allows a focused set of
  GPU/Metal IOKit user-client classes, including the modern
  `iokit-open-user-client` classes Metal uses to create devices,
  legacy `iokit-open` user-client and registry classes Apple GPU compute
  profiles still use while opening and enumerating devices, an
  `AGXAcceleratorG` registry-class prefix for Apple GPU generation names,
  explicit AGX/IOSurface open-time user-client allowances,
  IOSurface/IOAccelerator service classes, non-mutating Apple
  `system-graphics`
  and safety-inference Metal discovery dependencies, GPU tools Metal service
  discovery dependencies, AGXMetal driver bundle mapping, expanded Metal driver
  metadata properties, Metal service lookups including
  `com.apple.windowserver.active`, and named sysctl lookup plumbing used by
  Apple GPU-backed compute and runtime OS checks.
- Because this is in the base Seatbelt policy, it applies to generated macOS
  command sandboxes rather than only workspace-write profiles. The intent is to
  let sandboxed commands use GPU-backed compute
  frameworks such as Metal, MPS, MLX, and PyTorch MPS without disabling the
  filesystem/network sandbox.
- The allowance intentionally adds GPU-adjacent IOKit and macOS service lookup
  surface, but it avoids wildcard IOKit open actions, broad Metal IOKit
  connection predicates, writable-root expansion, network expansion, and
  unrestricted sysctl reads.
- Regression coverage lives in
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_metal_gpu_iokit_user_clients`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_core_metal_iosurface_iokit_access`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_metal_device_creation_iokit_user_clients`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_avoids_metal_iokit_message_filters_in_workspace_profile`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_metal_open_time_iokit_user_clients_without_wildcard_action`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_metal_enumeration_iokit_open_registry_classes`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_coreml_metal_legacy_iokit_user_clients`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_system_graphics_metal_device_dependencies`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_safety_inference_metal_discovery_dependencies`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_gputools_metal_service_discovery_dependencies`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_avoids_broad_metal_iokit_connection_predicates`
  and
  `codex-rs/sandboxing/src/seatbelt_tests.rs::base_policy_allows_named_sysctl_lookup_plumbing`.

### Feature toggles

- This fork carries `enable_mcp_approvals` as a Rick-owned feature.
- Persist it in config with:
  - `codex features enable enable_mcp_approvals`
  - `codex features disable enable_mcp_approvals`
- Override it for one run with:
  - `codex --enable enable_mcp_approvals`
  - `codex --disable enable_mcp_approvals`
- `codex features list` marks Rick-owned features with `(rick)`.

### Commit and intent guidance

- Conventional Commits guidance is first-class and enabled by default.
  Disable it with:

  ```toml
  [conventional_commits]
  enabled = false
  ```

- Git intent notes guidance is first-class and enabled by default. When enabled,
  workspace-write sessions add narrow write access to git note refs/logs and
  object storage when the git metadata resolves inside the trusted project, so
  agents can use `refs/notes/intention` without reopening all of `.git`. Gitdir
  layouts that escape the trusted project still use the normal rules or approval
  path.
  Disable the guidance entirely with:

  ```toml
  [git_intent_notes]
  enabled = false
  ```

- To keep the guidance but require normal approval/escalation for note writes,
  use:

  ```toml
  [git_intent_notes]
  allow_git_metadata_writes = false
  ```

### Collaboration modes

- Mainline removed the legacy `/collab` command; use `/plan` for Plan mode.
- The fork-only Orchestrator collaboration mode and `codex --collab <mode>`
  startup flag are removed.
- Legacy serialized `orchestrator` collaboration-mode values deserialize as
  Default mode for compatibility. App-server `thread/control/set` rejects
  Orchestrator mode.
- Standalone Continuous collaboration mode is removed. Use `/continuous` inside
  a normal session to enable or disable continuous execution for that thread.
- Multi-agent `spawn_agent` uses the mainline model-visible schema. Agents
  cannot request a child collaboration mode; child sessions use the normal spawn
  defaults and runtime controls.

### Orchestrator memory

- The legacy `[orchestrator_memory]` config remains for migration and
  compatibility, but live memory reads, writes, cleanup, and consolidation now
  use the `user_preferences` extension under
  `<codex_home>/memories/extensions/user_preferences`. New behavior should be
  configured through `[user_preferences_memory]`.
- Existing orchestrator-memory migration helpers can copy files from the legacy
  root, and from the pre-extension `<codex_home>/user_preferences_memory` root,
  into the canonical user-preferences extension. After that, legacy roots are
  not injected into model context.

### User preferences memory

- User preferences memory is the canonical live memory store for durable
  preferences, personal context, relational attunement, operator playbooks,
  ongoing threads, and follow-up state. It defaults to enabled and scoped to all
  collaboration modes.

  ```toml
  [user_preferences_memory]
  enabled = true
  scope = "all"
  read_buckets = [
    "durable_preference",
    "personal_context",
    "relational_attunement",
    "operator_playbook",
    "ongoing_threads",
    "followup_state",
  ]
  write_buckets = [
    "durable_preference",
    "personal_context",
    "relational_attunement",
    "operator_playbook",
    "ongoing_threads",
    "followup_state",
  ]
  model_on_heuristic_miss = false
  model_consolidation = false
  migrate_from_orchestrator_memory = false
  disable_orchestrator_memory_after_migration = false

  [user_preferences_memory.cleanup]
  enabled = true
  schedule = "03:30"
  run_missed_on_startup = true
  dedupe_raw_events = true
  deep_consolidation = true
  model_consolidation = true
  retain_forget_events_days = 30

  [memories]
  use_memories = true
  generate_memories = true
  extract_model = "gpt-5.4-mini"
  extract_reasoning_effort = "low"
  consolidation_model = "gpt-5.4"
  consolidation_reasoning_effort = "medium"
  ```

- The memory classifier is broader than task reminders: it should retain durable
  user preferences, working style, follow-up intent, operator playbooks, and
  other continuity notes when the user signals they matter later.
- Memory events carry applicability scope in addition to bucket. Non-global
  scopes use `repo`, `project`, `task`, `person`, `process`, `skill`,
  `command`, or `tool` identifiers so repo-, project-, task-, or
  process-shaped guidance does not silently become user-wide guidance.
  Summary/profile rendering prefixes scoped memories with `[type:id]`.
- `/orchestrator-memory-forget <needle>` removes matching orchestrator-memory
  entries from the canonical user-preferences memory store without touching
  mainline memory stores. Because this is a global text-prune maintenance
  command, the active session must have write access to all user-preferences
  buckets; narrower sessions should use normal in-turn forget requests for
  bucket-scoped behavior.
- `/orchestrator-memory-consolidate` triggers the configured orchestrator-memory
  cleanup/consolidation path against the canonical user-preferences memory store
  immediately, which is useful for testing cleanup behavior without changing the
  configured schedule. Like `/orchestrator-memory-forget`, the active session
  must have write access to all user-preferences buckets because consolidation
  rewrites the global event log and bucket mirrors.
- Explicit forget requests such as `forget this: ...` are treated as memory
  removal requests.
- To avoid silent background model spend, heuristic misses do not invoke a
  classifier model by default, and summary/profile consolidation uses the
  mechanical renderer by default. Set `model_on_heuristic_miss = true` or
  `model_consolidation = true` to restore those model-assisted paths. When
  `model_on_heuristic_miss = true`, scope-sensitive heuristic candidates are
  also routed through the classifier so richer buckets and narrower
  applicability scopes can be selected before writing.
- Memory events are mirrored into bucket-specific files under
  `<codex_home>/memories/extensions/user_preferences/buckets/` for easier
  inspection while preserving `preferences.jsonl` as the canonical event log.
- The outer `[memories]` read/write policy gates the whole memory layer:
  `use_memories = false` suppresses memory prompts, and
  `generate_memories = false` suppresses memory writes for the session. The
  app-server exposes the same session-local control through
  `thread/memoryPolicy/set`, and `thread/start`, `thread/resume`, and
  `thread/fork` accept `memoryPolicy`. A read-only policy adds memory roots as
  read-only sandbox roots; write access implies read access because writable
  filesystem roots are readable; a disabled policy does not add automatic
  memory sandbox roots.
- `read_buckets` controls which bucket sections a new session may inject into
  model context. `write_buckets` controls which buckets the session may update.
  Both can be narrowed per live app-server thread with
  `thread/userPreferencesMemoryPolicy/set`, and `thread/start`, `thread/resume`,
  and `thread/fork` accept `userPreferencesMemoryPolicy` to set the initial
  per-session policy.
- Scheduled cleanup runs at most once per day by local time, executes on the next
  startup if the scheduled time was missed, compacts duplicate raw events,
  retains recent forget tombstones, and resyncs bucket mirrors. By default it
  also runs a `Memory [memory builder]` sub-agent to merge semantic
  near-duplicates before regenerating summary/profile artifacts; set
  `cleanup.model_consolidation = false` for mechanical-only cleanup.
- Legacy memory events that predate bucketed schemas are migrated on the next
  write or consolidation, with a `preferences.jsonl.pre-bucket-migration`
  backup in `<codex_home>/memories/extensions/user_preferences`.
- Startup automatically copies missing files from the pre-extension
  `<codex_home>/user_preferences_memory` root into
  `<codex_home>/memories/extensions/user_preferences` when memory writes are
  enabled. Read-only sessions fall back to the legacy root without mutating it.
  Startup can also copy missing files from `<codex_home>/orchestrator_memory`; set
  `migrate_from_orchestrator_memory = true` to treat that pass as an
  orchestrator-memory migration. Set
  `disable_orchestrator_memory_after_migration = true` when you want the
  effective `orchestrator_memory` config disabled after that pass succeeds.
- `/user-preferences-memory-migrate` runs the same copy pass on demand for the
  current Codex home. It does not edit config; use the TOML option above when
  you want orchestrator memory disabled after migration.
- `<codex_home>/memories` is created and added as an automatic workspace-write
  root only when the outer memory write policy is enabled.

### Exec-policy rulesets

- Named exec-policy rulesets can be configured in `config.toml`:

  ```toml
  [exec_policy.rulesets.implementation-agent]
  mode = "exclusive"
  files = ["./implementation-agent.rules"]
  ```

- App-server `thread/start`, `thread/resume`, and `thread/fork` accept
  `execPolicy: { "rulesets": ["implementation-agent"] }` to select rulesets for
  that thread without changing cwd or editing the normal `.codex/rules` stack.
- Ruleset definitions must come from server config. When `execPolicy` selects a
  ruleset, request-local `config` overrides cannot define or replace
  `[exec_policy]`.
- `mode = "overlay"` keeps the normal user/project/system `.rules` files and
  applies selected rulesets on top. Unmatched commands use the existing
  safe/dangerous command heuristics.
- `mode = "exclusive"` loads only the selected ruleset files plus mandatory
  managed policy from requirements. Unmatched commands are forbidden, so this is
  the tight allowlist mode.
- A thread cannot combine overlay and exclusive rulesets. When matching rules
  conflict, the exec-policy engine keeps the existing strictest-decision
  behavior: `forbidden`, then `prompt`, then `allow`.

### Built-in scratchpad

- Default mode treats scratchpad as a first-class recovery ledger. Plan mode
  does not use built-in scratchpad by default.
- `/continuous` toggles a scratchpad-backed continuous run policy for the
  current thread. New thread scratchpads default to continuous mode unless
  `[scratchpad].default_continuous = false` or a mode override disables it.
  When `run_policy.continuous.enabled` is true and the scratchpad still has
  actionable `next_steps`, Codex loops back to the scratchpad instead of
  finalizing. Blocked work should be moved to `pending_waits`; pending waits
  alone do not keep continuous mode running.
- Model-capacity self-healing is opt-in and only applies while the current
  thread's continuous policy is enabled. When the backend reports "Selected
  model is at capacity," Codex waits for the configured interval and retries
  the same sampling request instead of ending the turn. The wait remains
  interruptible, and `/continuous off` prevents another retry after the
  current wait:

  ```toml
  [scratchpad.capacity_retry]
  enabled = true
  delay_minutes = 5
  ```

  The feature defaults to disabled; `delay_minutes` defaults to `5` and must
  be at least `1` when configured through `config.toml`.
- Automatic continuous scratchpad loopbacks are bounded by a rolling window.
  The default allows five loopbacks in a five-minute rolling window, then stops
  the current automatic continuation on the next attempt. A later turn can
  proceed once an older loopback has left the window. Configure the limit and
  window in `config.toml`:

  ```toml
  [scratchpad.loopback]
  max_loopbacks = 5
  window_minutes = 5
  ```

  `max_loopbacks` is clamped to `1` through `1024`, and `window_minutes` is
  clamped to at least `1`. The limit is tracked for the loaded thread session;
  changing either value while reloading config starts a fresh in-memory window.
- Scratchpads include `communication_policy` for durable communication
  preferences; channel failure alone is not treated as permission to stop or
  fall back to a final response.
- The fork has a canonical built-in `scratchpad` tool namespace. When a
  configured scratchpad MCP exposes the same namespace, the built-in namespace
  stays model-visible and its handlers take precedence.
- Agents receive mode-scoped developer guidance explaining when and how to use
  the built-in scratchpad. If built-in scratchpad is disabled for a mode, the
  tool namespace and guidance are both omitted for that mode.
- For action-oriented work, that guidance tells agents to treat `next_steps` as
  short-term working memory: add the initial task plan early, append newly
  discovered tasks/issues/tests/review follow-ups before they can be lost, and
  move finished work into `completed` instead of dropping it from the ledger.
- Built-in scratchpads are JSON-backed under `<codex_home>/scratchpad/entries`
  unless a tool call provides `state_home`. Scratchpad rollback checkpoints
  are stored separately under `<codex_home>/scratchpad/history` and are
  keyed by user-turn boundaries so thread backtracking can restore the full
  scratchpad document, including fields beyond `next_steps`.
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
  next-step, pending-wait, and blocked-item updates, action-policy checks, and
  wait check-ins. Continuous mode treats `next_steps` as the actionable queue;
  `completed` is the done ledger, while `pending_waits` and `blocked` are
  recovery context and do not keep the loop alive on their own. Use
  `wait_type = "user_confirmation"` for waits that need the user to grant
  access, confirm a decision, or merge/unblock something.
- `action_policy` is the session-scoped guardrail surface for user-stated PR,
  merge, deploy, benchmark, and AWS-write rules. Scratchpad guidance tells
  agents to persist those rules with `set_action_policy`, and active
  scratchpad recovery carries the policy back into context so agents can run
  `check_action_allowed` before taking guarded actions. It supports repo-level
  PR/merge deny flags, base-branch allow/deny lists, deployment environment
  allow/deny lists, and AWS-write default-deny behavior.
- The current thread scratchpad objective can be renamed through
  `update_scratchpad.objective`. `open_scratchpad` still refuses to rebind an
  existing thread-owned scratchpad to a different objective.
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
- Scratchpad fanout is opt-in. When enabled, developer guidance allows agents
  to delegate independent, disconnected `next_steps` to up to `max_agents`
  child agents while keeping the parent responsible for integration, follow-up
  fixes, merge safety, and instruction-compliance checks.
- `/scratchpad` renders the current session's built-in scratchpad on demand,
  including current objective, status, completed work, next steps, and waits.
  Structured waits prefer human-readable fields like `summary`, `description`,
  `reason`, and `details` instead of falling back to a generic pending-wait
  label.
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
  the agent can continue the same recovery ledger without searching. The
  injected state includes a bounded `recent_completed` list plus
  `completed_count`; the full `completed` ledger remains in the scratchpad
  file and `/scratchpad` output. Completed and archived scratchpads are skipped.
- When continuous mode prevents a final answer, the recovery prompt includes
  the current next steps, waits, and blockers so the agent can see stale or
  incomplete scratchpad state immediately.
- Scratchpad lifecycle cleanup runs mechanically during config load. By
  default, non-archived pads are archived after 30 days without updates, and
  archived pads are deleted after 90 days in the archive.
- Scratchpad rollback retention is bounded independently from transcript
  backtrack. Conversation backtrack remains able to walk the retained thread
  history, while scratchpad restore keeps the last
  `[scratchpad.rollback].max_user_turn_checkpoints` user-turn checkpoints. The
  default is 10; set it to 0 to disable scratchpad rollback checkpoints or
  raise it for deeper scratchpad restore history.
- After context compaction, actionable built-in scratchpad state is looped back
  through hidden developer context, using the same model-visible hidden-context
  path as other post-compaction recovery state rather than a synthetic user
  turn.
- Built-in scratchpad availability is controlled globally and per mode with:

  ```toml
  [scratchpad]
  enabled = true
  default_continuous = true
  recover_after_compaction = true
  auto_archive_after_days = 30
  delete_archived_after_days = 90

  [scratchpad.rollback]
  max_user_turn_checkpoints = 10

  [scratchpad.capacity_retry]
  enabled = false
  delay_minutes = 5

  [scratchpad.loopback]
  max_loopbacks = 5
  window_minutes = 5

  [scratchpad.fanout]
  enabled = false
  max_agents = 3

  [scratchpad.view]
  enabled = true
  show_id = true
  completed_items = 1
  next_steps = 5
  pending_waits = 5

  [scratchpad.modes.plan]
  enabled = false
  default_continuous = false
  recover_after_compaction = false

  ```

- The legacy top-level `[orchestrator]` mode config is removed after
  Orchestrator mode removal.

### Situational requirements

- Situational requirements are off by default. When enabled, Codex injects
  deterministic trigger/action rules into developer context so recurring guard
  actions are mechanically required when a situation applies.

  ```toml
  [situational_requirements]
  enabled = true

  [[situational_requirements.rules]]
  trigger = "code_change"
  actions = [
    { action = "git_intent_note", mcp = "git-intent-notes" },
  ]

  [[situational_requirements.rules]]
  trigger = "iac_change"
  actions = [
    { action = "aws_docs_check", mcp = "aws-docs" },
    { action = "post_change_review", skill = "post-change-review" },
  ]
  ```

- Supported triggers are `code_change`, `test_change`, `iac_change`,
  `doc_change`, `web_search`, and `pr_open`.
- Supported actions are `git_intent_note`, `aws_docs_check`,
  `post_change_review`, `skill`, `mcp`, and `web_search_citation`. Rules can
  name an MCP or skill so the model-visible requirement points at the exact
  guard surface to use.

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
- Built-in schedule is disabled by default. Default and Plan mode can opt in.
- Scheduled triggers are JSON-backed under `<codex_home>/schedule/triggers`
  unless a tool call provides `state_home`.
- `<codex_home>/schedule` is created and added to workspace-write writable roots
  automatically.
- Agents receive mode-scoped developer guidance explaining when to use schedule,
  when to prefer scratchpad pending waits instead, and how to link triggers to
  built-in scratchpad ids.
- Built-in schedule exposure is controlled globally and per mode with:

  ```toml
  [schedule]
  enabled = false

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
- Codex maintains a non-secret alias registry at
  `<codex_home>/accounts/registry.json` for UIs and app servers. It records the
  alias, label, storage home, auth file path, auth-file presence, credential
  store mode, source, and last-seen/last-used timestamps. It does not store
  tokens or account credentials.
- The registry self-heals from `[accounts]` config, existing
  `<codex_home>/accounts/<alias>` folders, the root default auth store, and
  first use of `--account` or `/account`, so keychain-only aliases can still be
  discoverable before an `auth.json` fallback file exists.

### MCP visibility and inventory

- `/mcp` includes mode-aware visibility in this fork so Codex can distinguish
  configured/available MCPs from MCPs hidden by the current mode.
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
