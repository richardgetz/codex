# Fork Delta Inventory

This file tracks fork-only changes that ship with this build. Keep it updated as
the fork evolves, and use it as a merge-awareness checklist whenever upstream
stable/mainline is pulled in.

## Introduced In 0.124.0-rick.2 (Recent)

- First-class commit and intent guidance:
  - Conventional Commits developer guidance is enabled by default.
  - Config: `[conventional_commits].enabled`, default `true`.
  - Git intent notes developer guidance is enabled by default.
  - Config: `[git_intent_notes].enabled`, default `true`.
  - Config: `[git_intent_notes].allow_git_metadata_writes`, default `true`.
  - When enabled, workspace-write adds narrow git metadata write roots for
    `refs/notes/intention`, note reflogs, and git object storage when the git
    metadata resolves inside the trusted project, without making `.git/config`
    or hooks writable.
- Decision provenance and crossroads:
  - Config: `[decision_provenance]`; both `enabled` and `git_intent_bridge`
    default to `false`.
  - When both are true, request-start preflight reads bounded local
    `refs/notes/intention` metadata for likely code/API/behavior/invariant or
    generated-file changes. Relevant `intent_priority: must` notes create an
    approval crossroad linked to the commit; only an explicit user override
    naming that Git intent records a user decision while preserving the
    earlier intent note as history.
  - The bridge is read-only with respect to Git notes and stores source
    references rather than duplicating note bodies. The canonical event log
    and materialized records remain in state SQLite; Inbound reads the
    versioned projection at
    `<state_home>/decision-provenance/projection-v1.json`.
- Account alias switching:
  - CLI: `codex --account <alias>`
  - In-session: `/account <alias>` and `/account default`
  - Behavior: alias auth resolves through `~/.codex/accounts/<alias>`, while
    the root auth store remains the default for compatibility with mainline
    Codex.
  - Storage policy: root/default auth stays file-compatible for mainline, while
    managed aliases default to keychain-first `auto` storage with file
    fallback when keychain is unavailable.
  - Non-secret alias registry:
    `<codex_home>/accounts/registry.json`
  - Registry behavior: self-heals from the root auth store, existing alias
    directories, `[accounts].active`, `[accounts].rotation`, and first alias
    use through `--account` or `/account`, so keychain-only aliases remain
    discoverable for app-server UIs even when no fallback `auth.json` exists.
- Managed session temporary storage:
  - Config: `[session_tmp]`; `enabled` defaults to `false`, `root` defaults to
    `<codex_home>/session-tmp`, and `stale_after_days` defaults to `7`.
  - When enabled, each root session and spawned agent receives an isolated
    managed directory with durable path lineage and ownership metadata. Only
    managed-layout paths are eligible for cleanup; agents are told that all
    files under their managed directory are disposable and must not store
    durable artifacts, credentials, or source files there.
  - Slash command: `/tmp [status|list|clean|clear|reap [days]]`. The current
    root session owns cleanup; `clear` also removes manual-retention entries,
    while `reap` force-cleans only sessions older than the selected age.
- Local token usage and spend tracking:
  - `/status` can show API-equivalent token usage and estimated cost when
    `[tui.status_token_usage].enabled = true`.
  - `/spend [days|YYYY-MM|YYYY-MM-DD..YYYY-MM-DD]` renders local daily spend
    rollups from `<codex_home>/usage/daily_spend.json`.
  - Config: `[tui.status_token_usage]` with `daily_spend_retention_days`
    (default `30`) and per-model `model_rates`/`service_tiers` overrides in USD
    per 1M tokens. Estimates are not billing statements.
- Managed ChatGPT reauthentication recovery:
  - A permanently failed refresh no longer causes the TUI to treat a stale
    managed OAuth keychain entry as a completed ChatGPT login. Choosing ChatGPT
    starts a fresh browser/device flow and lets the new credential replace the
    old keychain entry without requiring manual deletion.
  - Embedded TUI sessions open the login URL locally; remote app-server clients
    use the device-code/headless flow or explicitly forward the app-server
    callback port because the callback URL belongs to the remote host.
- Removed collaboration-mode remnants:
  - Mainline `/collab` remains absent; use `/plan` for Plan mode.
  - Fork-only `codex --collab <mode>` startup selection is removed.
  - Fork-only Orchestrator collaboration mode is removed. Legacy serialized
    `orchestrator` mode values deserialize as Default for compatibility.
  - App-server `thread/control/set` rejects Orchestrator mode.
- Orchestrator memory compatibility:
  - `[orchestrator_memory]`
  - The legacy config and migration helpers remain, but live read/write,
    cleanup, consolidation, and context injection use
    `<codex_home>/memories/extensions/user_preferences`.
- User preferences memory maintenance:
  - Slash command: `/orchestrator-memory-forget <needle>`
  - Slash command: `/orchestrator-memory-consolidate`
  - Bucket-specific mirror files live under
    `<codex_home>/memories/extensions/user_preferences/buckets/`.
  - Memory events carry applicability scope separately from bucket:
    `global`, `repo`, `project`, `task`, `person`, `process`, `skill`,
    `command`, or `tool`; non-global entries render with `[type:id]` so
    narrower guidance is not treated as user-wide by accident.
  - Scheduled cleanup runs daily by local `HH:MM` schedule, defaults to `03:30`,
    compacts duplicate raw events in `preferences.jsonl`, keeps recent forget
    tombstones, resyncs bucket files, and defaults to a `Memory [memory builder]`
    semantic merge pass before regenerating summary/profile artifacts.
  - Legacy unbucketed memory events are migrated on next read/consolidation with
    a `preferences.jsonl.pre-bucket-migration` backup.
- User preferences memory:
  - Config: `[user_preferences_memory]`
  - Defaults: `enabled = true`, `scope = "all"`.
  - Stores under `<codex_home>/memories/extensions/user_preferences`; the outer
    `[memories]` policy controls automatic memory sandbox roots.
  - Startup automatically copies missing files from the pre-extension
    `<codex_home>/user_preferences_memory` root into the extension root when
    memory writes are enabled; read-only sessions can still read the legacy root
    without mutating it.
  - Config: `[memories]` supports `extract_model`,
    `extract_reasoning_effort`, `consolidation_model`, and
    `consolidation_reasoning_effort` for the main memory agents.
  - App-server outer memory access control: `thread/start`, `thread/resume`,
    and `thread/fork` accept `memoryPolicy`; loaded threads can be changed live
    with `thread/memoryPolicy/set`. Write access implies read access because
    writable memory roots are readable filesystem roots.
  - `read_buckets` and `write_buckets` default to all bucket types:
    `durable_preference`, `personal_context`, `relational_attunement`,
    `operator_playbook`, `ongoing_threads`, and `followup_state`.
  - When `model_on_heuristic_miss = true`, scope-sensitive heuristic memory
    candidates are routed through the model classifier so richer buckets and
    repo/project/task/process/person/tool scope can be selected before writes.
  - App-server: `thread/start`, `thread/resume`, and `thread/fork` accept
    `userPreferencesMemoryPolicy`; loaded threads can be changed live with
    `thread/userPreferencesMemoryPolicy/set`.
  - Startup copy migration is available with
    `migrate_from_orchestrator_memory = true`.
  - `disable_orchestrator_memory_after_migration = true` disables the effective
    orchestrator-memory config after that copy pass succeeds.
  - Slash command: `/user-preferences-memory-migrate` copies missing files from
    `<codex_home>/orchestrator_memory` into
    `<codex_home>/memories/extensions/user_preferences` without editing config.
- Mode-scoped enablement filters:
  - `[enablement.modes.<mode>]`
  - Supports `skills`, `mcps`, and `plugins`
  - Each filter uses `{ mode = "include"|"exclude", items = [...] }`
  - `items = ["*"]` is supported
- Session-scoped agent pruning:
  - Slash command: `/agents-prune`
  - CLI: `codex agents-prune <thread-id> --remote <ws://host:port>`
    sends the same prune request to a long-lived remote app-server without
    opening a TUI.
  - Closes idle spawned agents from the current session's shared agent control
    registry and live thread-spawn tree only.
  - Preserves running and initializing agents, the current thread, and any
    agent subtree that still contains active work.
- MCP visibility recovery:
  - Cancelled MCP startups are retried in a bounded way instead of memoizing the
    cancelled startup for the rest of the session.
  - Plain unavailable MCP placeholder calls such as
    `mcp__aws_auth_guard__auth_guard_status` are mapped back to configured MCP
    servers, forcing a server tool-list/start path and resolving the real MCP
    tool when the daemon is available.
  - The model-visible MCP inventory is based on configured/started direct
    servers plus unstarted lazy servers, not only successful tool listings, so
    eager MCPs remain visible even when their current tool list is temporarily
    unavailable.
- Built-in scratchpad:
  - Namespace: `scratchpad`
  - Default mode exposes it by default; Plan mode does not.
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
  - Keys: `enabled`, `default_continuous`, `recover_after_compaction`,
    `auto_archive_after_days`, `delete_archived_after_days`
  - Config: `[scratchpad.fanout]`, default `enabled = false`,
    `max_agents = 3`; when enabled, developer guidance allows fanout of
    independent disconnected `next_steps` while keeping the parent as
    integrator/checker.
  - Built-in scratchpad tools are bound to the current thread/session id:
    `open_scratchpad` defaults `scratchpad_id` to that id, and model-visible
    tools reject custom or other-thread scratchpad ids.
  - `resume_scratchpad` strictly reopens the current thread scratchpad without
    creating a replacement; archived pads remain readable/editable by their
    owning thread until lifecycle deletion.
  - Slash command: `/scratchpad` renders the current session scratchpad on
    demand with the full completed, next-step, and pending-wait lists.
    Structured waits render human-readable `summary`, `description`, `reason`,
    and metadata fields instead of a generic pending-wait label.
  - Slash command: `/scratchpad-absorb <scratchpad_id>` copies another
    scratchpad into the current thread scratchpad as contextual history without
    changing source ownership or importing live control policy. It includes
    pending waits by default; `--exclude-pending` omits them.
  - Slash command: `/scratchpad-unarchive` clears the archived marker on the
    current thread scratchpad so it is no longer eligible for archived-pad
    cleanup.
  - Slash command: `/outcomes` renders measured scratchpad outcomes as a
    markdown postmortem summary.
  - Built-in scratchpad tools include `record_outcome` and `export_outcomes` for
    portable, scoped progress measurements with metric/unit,
    baseline/current/delta, summary, tradeoffs, artifact, commit, and PR
    provenance.
  - Live TUI scratchpad update cards are configurable through
    `[scratchpad.view]`: `enabled`, `show_id`, `completed_items`,
    `next_steps`, and `pending_waits`. Defaults keep live cards visible, show
    the id, show only the newest completed item, and show five next steps and
    waits.
  - Slash command: `/continuous [on|off|status]` toggles
    `run_policy.continuous.enabled` on the current thread scratchpad. New
    thread scratchpads default to continuous mode unless
    `[scratchpad].default_continuous = false` or a mode override disables it.
    When it is enabled and the scratchpad still has actionable `next_steps`,
    Codex loops back to continue instead of finalizing. Blocked work belongs in
    `pending_waits`; pending waits alone do not keep continuous mode running.
  - Config: `[scratchpad.capacity_retry]`, with `enabled = false` and
    `delay_minutes = 5` by default. When enabled, model-capacity errors retry
    after the configured delay only while the thread's scratchpad continuous
    policy remains enabled; the wait is interruptible and rechecks the live
    policy before retrying.
  - Config: `[scratchpad.loopback]`, with `max_loopbacks = 5` and
    `window_minutes = 5` by default. Continuous mode stops before another
    automatic loopback when the configured rolling-window limit is reached;
    the limit is tracked for the loaded thread session.
  - Scratchpads support standalone `communication_policy` fields for durable
    communication preferences; channel failure alone must not force a final
    response while the main work can continue.
  - Tool: `record_delegation` records parent scratchpad lineage for work
    delegated to subagents, including subagent id/label, parent item refs,
    child scratchpad id, status, notes, and artifacts.
  - Config: `[scratchpad].outcomes_enabled` defaults to `false`; `/outcomes on`
    and `/outcomes off` persistently toggle it in config.toml. When disabled,
    `record_outcome` refuses new datapoints while `/outcomes` can still export
    existing entries.
  - Legacy `continuous` collaboration-mode values in old config or rollout
    payloads deserialize as `default` for compatibility only; they do not enable
    continuous policy. Use `/continuous on` for the scratchpad-backed runtime
    behavior.
  - Resume injects the active thread scratchpad id and compact scratchpad state
    into hidden developer context when the thread-id scratchpad exists with
    uncompleted work (`next_steps` or `pending_waits`).
  - Continuous-mode recovery prompts include the current next steps, waits, and
    blockers so stale or incomplete scratchpad state is visible when a final
    answer is blocked.
  - Supports active/archived lookup, archive/unarchive, next-step and
    pending-wait updates, blocked-item updates, action-policy checks, and wait
    check-ins.
  - Lifecycle cleanup runs during config load. Defaults: archive non-archived
    pads after 30 days without updates; delete archived pads after 90 days in
    archive. Set either day value to `0` to disable that phase.
  - Rollback journals are bounded by both the configured checkpoint count and
    a 32,000-token serialized-size budget. When snapshots are large, the
    oldest checkpoints are evicted first so recent recovery state is retained.
  - Scratchpad writes coordinate through a per-state-home cross-process file
    lock and durable atomic replacement; interrupted writes leave a recoverable
    journal rather than a partially written JSON file.
- Situational requirements:
  - Config: `[situational_requirements]`, default `enabled = false`.
  - Rules map triggers such as `code_change`, `test_change`, `iac_change`,
    `doc_change`, `web_search`, and `pr_open` to actions such as
    `git_intent_note`, `aws_docs_check`, `post_change_review`, `skill`, `mcp`,
    and `web_search_citation`.
  - Enabled rules are injected as deterministic developer requirements and can
    name the expected MCP or skill guard surface.
- Post-compaction recovery:
  - Config: `[scratchpad].recover_after_compaction` and
    `[scratchpad.modes.<mode>].recover_after_compaction`
  - Default: `true`
  - In scratchpad-enabled modes, actionable built-in scratchpad state is looped
    back through hidden developer context after compaction. Completed or
    archived scratchpads are not looped back, and the TUI does not synthesize a
    user turn for recovery state.
  - Legacy top-level `[orchestrator]` mode config is removed after
    Orchestrator mode removal.
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

- Memory helpers have human-readable names:
  - `Memory [extractor]`
  - `Memory [memory builder]`
- Collaboration-mode skill filtering exists and now rolls up under the unified
  enablement model.

## Merge Checklist

- Verify `/plan` still enters Plan mode and no `/collab` command is exposed.
- Verify `codex --collab ...` is rejected and legacy serialized `orchestrator`
  collaboration-mode values map to Default.
- Verify `codex --account ...` and `/account ...` still switch auth stores
  without breaking the default root auth location.
- Verify `/orchestrator-memory-forget <needle>` still prunes and reconsolidates
  orchestrator memory, including bucket mirror files.
- Verify `/orchestrator-memory-consolidate` still triggers a manual
  orchestrator-memory cleanup pass.
- Verify `[enablement.modes.<mode>]` still filters `skills`, `mcps`, and
  `plugins` correctly.
- Verify cancelled MCP startup can retry, a plain unavailable MCP placeholder
  call can recover the configured server namespace instead of permanently
  reporting the tool unavailable, and eager MCP servers remain listed in the
  model-visible inventory even if tool listing is temporarily unavailable.
- Verify app-server `thread/control/set` rejects Orchestrator mode.
- Verify built-in `scratchpad` remains available in Default mode, omitted from
  Plan mode by default, and `open_scratchpad` uses the thread id when no id is
  provided.
- Verify `/continuous` can be toggled on/off while a model turn is running and
  updates the current thread scratchpad without queuing a core op.
- Verify built-in `resume_scratchpad` refuses to create a new scratchpad,
  archived pads remain same-owner readable/editable, and model-visible
  scratchpad tools reject custom or other-thread scratchpad ids.
- Verify `/scratchpad-absorb` writes only to the current thread scratchpad,
  preserves source ownership, and does not import live control policy.
- Verify `/scratchpad-unarchive` clears the archived marker only on the current
  thread scratchpad.
- Verify configured scratchpad MCPs do not shadow the built-in scratchpad
  namespace.
- Verify post-compaction built-in scratchpad loopback is hidden from the TUI and
  only injects actionable scratchpads with `next_steps` or `pending_waits`, not
  completed or archived scratchpads.
- Verify memory helper naming still shows `Memory [extractor]` and
  `Memory [memory builder]`.
- Verify first-class Conventional Commits and git intent notes guidance appears
  by default, can be disabled by config, and intent-note metadata access does
  not make `.git/config`, hooks, or escaped linked-worktree metadata writable.
