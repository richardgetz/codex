# Fork Delta Inventory

This file tracks fork-only changes that ship with this build. Keep it updated as
the fork evolves, and use it as a merge-awareness checklist whenever upstream
stable/mainline is pulled in.

## Maintenance Rule

Every fork-only feature or behavior change must update this inventory in the
same change. This includes fork-specific defaults, commands, configuration,
MCP or skill behavior, API surfaces, persistence/compatibility behavior, and
release or merge rules.

- Add a concise entry under `## Unreleased` (or move it into the applicable
  release section when the fork version is cut).
- Add or update a `Merge Checklist` item when an upstream refresh could remove
  or regress the behavior.
- If the change is not fork-only, explicitly confirm in the pull request that
  no inventory entry is needed.

## Unreleased

- Fork distribution and release contract:
  `@rickgetz/codex`/`codex-rick`, `-rick.<counter>` versions and `rick-v...`
  tags, stable-triggered releases, Apple Silicon lane, and migration-number
  policy remain fork-owned (see the release and migration docs).
- macOS Seatbelt GPU/Metal base-policy allowances preserve focused IOKit,
  service, and sysctl access for sandboxed MPS/MLX/PyTorch workloads with
  deny-wildcard regression coverage.
- Rick-owned `enable_mcp_approvals` feature toggles and `(rick)` owner labels
  remain on fork-only experimental help and announcements.
- Native GPT-Live voice in the TUI remains fork-owned: WebRTC V3 transport,
  microphone/speaker controls, voice rotation, handoff classification and
  preamble policy, bounded diagnostics/history, and realtime configuration.
- Named exec-policy rulesets (`overlay`/`exclusive`) remain selectable through
  app-server `execPolicy` and server config.
- Fork-aware help context and fork-only feature labeling keep
  `docs/fork-differences.md` current and identify Rick-owned metadata.
- Bounded fork-help context keeps a compact `/account <alias>` and
  `/orchestrator-memory-forget <needle>` command index ahead of the
  8,000-token middle-truncated inventory so essential fork commands remain
  discoverable while the full inventory stays the source of truth.
- Initial developer context keeps extension Skills world-state sections ahead of
  Apps and Plugins usage guidance, while preserving the existing App enablement,
  model-capability, and connector filtering rules.
- Cancellation at a tool activity or parallel-dispatch boundary returns the
  normal aborted response; when cancellation and a pre-admission dispatch are
  both ready, cancellation claims the terminal outcome first while already
  claimed completions and genuine task-join failures remain observable.
- TUI team waits publish the waiting header before updating interruption hints,
  and collaboration-mode discovery keeps an empty server catalog empty instead
  of synthesizing built-in presets; visible modes still follow server filtering.
- Main-checkout Rust build coordination: one designated build owner runs
  serialized Cargo/`just` validation against one shared target/cache after
  source integration; worker worktrees remain source-only, and active
  targets/worktrees are preserved.
- Fork-preserved update-plan surface:
  `[tools.update_plan].enabled` remains default-on for stable compatibility;
  explicit `false` still removes `update_plan` from registered and visible
  tools, while explicit `true` retains it. The schema and config tests pin
  this default so upstream opt-in refreshes do not silently change the fork.

- Opt-in Lead/Worker model teams:
  - Team On is the user's explicit in-thread authorization to delegate
    substantive in-scope implementation, testing, research, and applicable
    skill work; Lead balance `1` favors Worker handoff with minimal optional
    Lead oversight while task-specific user, AGENTS.md, skill, scope,
    concurrency, depth, and approval restrictions remain authoritative.
  - `[team]` can define exactly one Lead and one Worker model/effort profile;
    profiles remain disabled for new sessions unless `team.enabled = true`.
  - `/team on`, `/team off`, and `/team status` switch and report the live
    per-thread assignment without changing global config defaults. `/team lead`
    and `/team worker` open the existing model/effort picker; typed forms accept
    an exact catalog model and effort. Profile changes patch only the current
    thread snapshot, leave Team Off unchanged, survive resume/fork, and apply
    to Workers spawned afterward while in-flight Workers stay pinned. Re-enabling
    Team mode while a Lead is parked with direct Workers starts a fresh oversight
    interval after the assignment is published.
  - `[team.lead].balance` defaults to `3` and accepts only `1..5`. `/team balance`
    opens a five-choice Lead usage/confidence picker and `/team balance 1..5`
    accepts a typed selection. The value is a session snapshot override that
    survives resume/fork and does not mutate global config or Team On/Off state.
    It changes only discretionary Lead oversight; level `3` preserves today's
    behavior exactly, while Workers retain full scope, completeness, required
    checks, approvals, and configured efforts.
    This advisory control makes no hard token-savings or correctness guarantee.
    It remains independent of `dynamic_handoff`, leaves
    `oversight_timeout_minutes` unchanged, and adds no polling loop.
  - Root sessions use Lead and delegated ThreadSpawn/review sessions use
    Worker; model and effort overrides cannot promote or bypass that assignment.
  - Routing enforces the selected catalog model and effort. It does not provide
    a hard tool sandbox or attest that an external skill completed.
  - `[team.lead].dynamic_handoff` defaults to `false`. When true, the Lead
    preflights before bulk log/trace, web/browser, or broad code/docs/repo
    lookup and routes only work where Worker filtering reduces Lead context;
    small or Lead-context-heavy lookups stay direct. Worker reports include
    concise answers, selected evidence excerpts, and file/line/time/source
    pointers, preserve uncertainty, and avoid full dumps. The Lead does not
    repeat supported findings automatically; follow-up is limited to concrete
    gaps or conflicts, blocked or incomplete Workers, or narrow excerpt
    requests, reusing prior findings. The choice persists in thread team
    snapshots; legacy snapshots default to false. This guidance is advisory,
    retains normal delegation limits, and Worker processing still consumes
    tokens.
  - Root Fast/service-tier changes propagate to loaded direct and nested
    ThreadSpawn Workers' settings snapshots and client notifications. In-flight
    turns keep their captured request tier, while later and newly spawned turns
    use the root selection.
  - `[team.worker].max_concurrent` optionally sets a positive, atomic ceiling
    for active direct Workers per Lead across V1 and V2. Pending starts reserve
    capacity, followups reacquire it, completed or aborted Workers release it,
    and grandchildren are excluded. Existing global agent-count, depth, and
    resource limits still apply independently; this setting does not raise or
    replace them. Leads receive bounded guidance that the setting is a ceiling
    rather than a target.
  - `[team.lead].oversight_timeout_minutes` defaults to `30` minutes and must
    be in the range `1..15768000` minutes (up to 30 years). A Lead parks
    without polling or automatic inference while direct Workers run; routine
    progress stays in a bounded (32-update,
    8-KiB) summary and does not wake or grow Lead context. The standalone
    `send_message_action` tool wakes the Lead for explicit action, while the
    reserved `collaboration.send_message` surface keeps its pre-team schema
    and queues routine progress. Handoff/completion, escalation/failure, user
    input, or the one oversight deadline for the current parked interval wakes
    the Lead. Routine progress never extends the current deadline; after a
    genuine Lead assessment, a new
    parked interval may arm another configured deadline. Interrupt, shutdown,
    and `/team off` cancel it and invalidate stale callbacks. A deadline
    warning and next-deadline state are visible to clients, and no deadline is
    armed when no direct Worker remains. Calling `wait_agent` while direct
    Workers run enters the same interval and ignores shorter per-call
    timeouts. Deadline state includes a readable RFC3339 UTC timestamp. A
    configured blocking Stop hook remains actionable and may require a Lead
    continuation before idle parking; successful lifecycle hooks do not create
    a routine polling turn. `/team off` drops pending automatic Worker wakeups
    while preserving queue-only mail. Automatic Lead trigger admission is
    serialized with assignment changes: stale triggers are discarded after
    Team Off, queue-only mail is retained, and already-admitted turns may
    finish with their captured settings. The legacy V1 `multi_agents.send_input`
    surface retains its explicit turn-input semantics and can wake a target;
    those task inputs are not reclassified as routine progress.
  - Automatic Goal continuations use the same Lead admission boundary and stay
    parked while direct Workers remain active; completion or actionable input
    rechecks the Goal without adding a polling turn.
  - Ordinary Goal continuations park while the active Goal turn owns tracked
    unified-exec processes, wake once exact processes terminate or are
    released. A monotonic intent generation rejects stale terminal callbacks
    after same-ID objective edits or Goal replacement, while turn/Goal
    cancellation and user input invalidate waiters without stopping the
    external process.
  - Selected integer-valued wait arguments accept exactly integral decimal or
    exponent spellings using their raw numeric lexemes; nested `Value` fields,
    fractional values, unsafe integers, and handler bounds remain unchanged.
  - Backports upstream Goal empty-continuation protection from PR #44320:
    automatically admitted Goal turns with no non-commentary activity block
    after three consecutive empty responses, while tool, reasoning, user,
    objective, and error activity resets the streak. Tracked external waits
    remain event-driven and do not admit a continuation until their wake.
  - Backports upstream explicit pause semantics from PR #44290: `update_goal`
    accepts `paused` only for an explicit user request, accounts final progress
    with budget limits taking precedence, and keeps resume and system-limit
    statuses host-controlled.
  - A Team Worker entering `wait_agent` with no active child dependency or
    queued activity queues one bounded handoff to its immediate parent. The
    handoff requests review or follow-up without marking the Worker complete;
    a per-turn latch suppresses repeated waits until meaningful parent input
    or new work arrives. V2 waits for currently admitted model/tool operations
    to quiesce and rechecks approval, user-input, usage, mailbox, and child
    state before sending; cancellation or another wait signal wins without a
    handoff before quiescence and the final boundary. Spawned non-wait sibling
    dispatches are counted from task creation through completion or cancellation,
    including readiness-blocked and quiescent exec/MCP wrappers; the coordination
    wait itself is excluded. The signal still only covers dependencies visible at
    this boundary, so work admitted afterward and sibling waits are not inferred.
    Legacy V1 explicit-target waits remain result collection and do not emit
    this dependency-free handoff.
  - `[team.lead].show_idle_notifications` defaults to `false`. When enabled,
    passive idle and parked-wait notices are emitted for debugging; actionable
    oversight-deadline warnings and wakes remain visible regardless. The global
    config preference is not stored in thread team snapshots, so resumed threads
    follow the current config value.
  - Team admission follows ordinary multi-agent backend compatibility, so a
    V2 Lead can use a V1 Worker; invalid active assignments fail open for root
    startup/resume with a warning and per-thread `off` mode, while delegated
    Workers and live toggles remain strict. A V1 Worker under a V2 Lead can
    use the selected collaboration namespace when its catalog metadata supports
    delegation, so it may spawn nested Workers; disabled assignments remain
    collaboration-tool-free.

- Advisory crossroads and decision-history traversal:
  - Request-start provenance matches no longer block model flow or infer user
    approval. Matching guidance is bounded informational context; independent
    permissions and explicit user instructions still apply.
  - `/decisions crossroads`, `show`, and `history` expose candidate sources,
    linked records, and review history. `reviewed`, `dismiss`, and `revisit`
    change bookkeeping only, not execution, approval, or released code.
  - Repeated requests reuse candidate records; distinct requests remain
    separate. ID prefixes resolve against the full store with ambiguity checks.
  - Existing records and opt-in defaults are preserved. Legacy approval
    crossroads remain separately queryable; retry deduplication applies to new
    informational candidates without inheriting legacy review states.
  - Semantic discussion and replacement-decision recording are not part of
    this foundation.

- Per-thread usage visibility, budgets, and reset-aware auto-resume:
  - Setup examples, defaults, and recovery limits are documented in
    [Fork differences](../../../../docs/fork-differences.md#per-thread-usage-budgets-and-automatic-resume-after-reset).
  - App-server v2 exposes a persisted `usagePolicy` on thread start, resume,
    fork, and settings-update surfaces. It is disabled by default per thread.
  - `autoResume` opts a thread into reset-aware continuation, while
    `minimumRemainingPercent` stops automatic continuation, loopbacks, hooks,
    and queued automatic work before the configured provider-window floor is
    crossed. Explicit user turns remain allowed.
  - Raw response-item injections received after a final answer still reopen the
    active turn for one follow-up request, while remaining automatic input for
    usage-floor admission and never bypassing `minimumRemainingPercent`.
  - The model receives bounded advisory status for provider usage windows,
    including remaining percentage and reset time for 5-hour, weekly, and
    other known windows.
  - Resettable provider limits can be retried after reset with cancellation
    awareness and a bounded retry count. The opted-in scheduler refreshes the
    authenticated account at most once per configured mechanical interval
    (default 60 minutes), checking sooner when a known reset falls inside that
    interval. Workspace or credit-cap failures are not automatically retried.
  - `[tui.usage_auto_resume]` provides an opt-in default for new root sessions
    and a validated 1-minute-to-7-day fallback interval. `/usage auto-resume
    on|off|status` changes or reports the displayed thread policy; a root
    change propagates to loaded ThreadSpawn descendants and future children,
    while a Worker change remains scoped to that Worker. Native `/continue` and
    `thread/activity/continue` resolve a viewed Worker to its Lead root, release
    that activity tree, and wake existing usage waits without creating a model
    turn. The usage-only `thread/usage/resume` request remains wake-only for
    the requested thread's loaded usage subtree and does not release a manual
    activity pause. Floor-paused automatic work uses the same scheduler;
    completed, cancelled, and manually stopped work is never revived.
  - The policy is preserved through resume, copied/reference/paginated forks,
    Last-N forks, and spawned subthreads. The persisted policy survives a cold
    resume, but an in-flight reset wait is process-local.
  - A hard API-equivalent dollar cap is not enforced because ordinary provider
    responses do not expose authoritative spend limits; local spend estimates
    remain informational.

- Session-scoped cooperative activity pause:
  - `/pause` and `/continue` pause or release the current Lead tree, including
    loaded direct and nested ThreadSpawn Workers; a viewed Worker resolves to
    its Lead root. New children reconcile the root state before admitting work.
  - The process-local pause gates future model/tool starts, usage-reset wakeups,
    and Lead oversight deadlines. Already-admitted side-effectful operations
    may finish at a cooperative boundary; external subprocesses or remote jobs
    are not suspended or replayed. Waiting for approval, user input, usage, or
    another agent is quiescent and reports `paused`.
  - Cancellation that arrives while a tool is waiting at an activity or
    parallel-dispatch boundary returns the normal aborted tool response instead
    of leaking the internal `TurnAborted` boundary error.
  - App-server v2 exposes `thread/activity/pause`,
    `thread/activity/continue`, `thread/activity/read`, and the ephemeral
    `thread/activity/updated` notification with structured activity, pause
    state, wait reason, and in-flight operation count. Continue releases the
    retained scheduler and nudges an existing usage wait without a synthetic
    model turn. Activity state is process-local and is not restored after a
    cold resume; completed, cancelled, and manually stopped work is never
    revived. The MCP `tools/call` runner forwards activity updates as
    notifications while retaining its existing turn completion semantics.
  - The native TUI renders the selected tree as two aligned rows: `Lead:
    idle|working|waiting · Team: N working[, M waiting]`, followed by
    `Workers: N[/cap] · Subagents: M`. Team counts include all unfinished
    direct and nested Workers; Workers counts only unfinished direct Lead
    children, using the optional `[team.worker].max_concurrent` ceiling as the
    denominator; Subagents counts deeper descendants. Completed, closed, and
    unrelated roots are excluded, and the same event-driven projection drives
    the terminal title. Working Lead/Workers animate, approval/user-input/
    usage/agent waits remain static, `Pausing` animates while aggregate
    in-flight operations drain, and `Paused · Lead + N workers · /continue to
    resume` is static, with `N` equal to the total unfinished Workers.
    Reduced-motion settings disable animation; no polling is added. Ordinary
    coordination waits retain a display-only working state for 30 monotonic
    seconds after the first Working-to-Waiting transition. Repeated waiting
    events do not extend that grace, and a bounded one-shot redraw reveals
    expiry without a new event or animation. Approval, user-input, usage-limit,
    error/completion, close, and pause transitions remain immediate. Parent
    edges come from existing thread metadata; no activity protocol fields or
    backend polling are added. Terminal completion wins over delayed activity
    updates until the next `turn/started`; reset/reconnect rebuilds metadata
    only for the selected loaded tree, ignores `NotLoaded`, ephemeral, or
    temporary helper threads, and rejects unknown/unloaded child activity until
    a fresh `thread/started`/`turn/started` admits it.
  - Collab spawn and V2 `SubAgentActivity` start/completion events locally admit
    their parent edges before persisted metadata arrives. Same-root metadata
    refreshes retain a provisional edge for a bounded grace window, then keep it
    only while an active direct or nested Worker entry remains; idle or absent
    omitted edges are pruned without polling. This keeps unfinished direct and
    nested Workers visible when `ThreadStarted` metadata is delayed.

- Recursive per-response usage accounting:
  - App-server v2 sends the legacy context-window counters through
    `thread/tokenUsage/updated` and a separate complete billing baseline through
    `thread/tokenUsageProjection/updated`. The projection reconstructs the root
    and recursively reachable agent sources from exact persisted response
    records, deduplicating by source thread and response ID while preserving
    model/provider/tier, context length, timestamps, parentage, and fork metadata.
  - A non-null empty projection is a complete zero baseline; `null` means the
    history read was unavailable. Spawned Workers, nested descendants, archived
    threads, and one-shot Review responses remain attributable across cold
    resume and live completion notifications. Fork context records do not count
    as the fork's own spend, and legacy aggregate daily history is preserved.
    Exact live completions are additive while the persisted projection is
    pending or unavailable, with deduplicated model breakdowns shown before the
    complete baseline arrives. Initial projection reads use bounded startup
    retries; persistent unreadable history remains explicitly unavailable.

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
    generated-file changes. Matching notes create bounded informational
    retrieval candidates linked to the commit; they never gate model flow or
    infer approval. `/decisions crossroads` and `/decisions show` provide
    source, option, relationship, and history traversal; reviewed, dismissed,
    and revisited states are bookkeeping only.
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
  - Cached normal tool-plan construction registers the complete per-step MCP
    inventory, including recovered placeholders, while retaining handler reuse
    keyed to the immutable MCP binding.
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

- Verify upstream refreshes preserve the main-checkout, single-owner,
  serialized Cargo workflow, source-only worker worktrees, integrated-source
  freeze with exact-revision handoff, and preservation of active
  targets/worktrees.

- Verify the fork distribution/release contract (`@rickgetz/codex`,
  `codex-rick`, `-rick.<counter>` versions, `rick-v...` tags, stable-triggered
  Apple Silicon releases) and migration-number policy remain intact.
- Verify the macOS Seatbelt GPU/Metal base-policy allowances and focused
  regression tests survive upstream policy changes without wildcard access.
- Verify `enable_mcp_approvals` remains a Rick-owned toggle and fork-only
  experimental help/announcements retain the `(rick)` owner label.
- Verify GPT-Live voice/device controls, WebRTC V3 handoff classification,
  preamble behavior, bounded diagnostics/history, and realtime config remain
  available in the native TUI.
- Verify named exec-policy rulesets retain their `overlay`/`exclusive`
  semantics and app-server `execPolicy` selection.
- Verify fork-aware help continues to load the checked-in fork differences and
  `(rick)` feature labeling remains applied to fork-only metadata.
- Verify fork-help remains bounded at 8,000 tokens while its compact command
  index survives middle truncation and the checked-in inventory remains the
  source of truth.
- Verify initial context preserves Skills → Apps → Plugins ordering without
  changing App enablement or connector filtering.
- Verify `[team]` rejects enabled configurations without both complete profiles,
  remains disabled by default, and `/team` state survives resume/fork without
  mutating global config. Verify Lead routing, Worker routing for all delegated
  and review sessions, nested Worker depth handling, override rejection, and
  single-model restoration after `/team off`. Verify `/team lead` and
  `/team worker` reuse the model/effort picker, typed commands accept only
  supported catalog pairs, Team Off remains unchanged during profile edits,
  updated profiles apply to newly spawned Workers, and in-flight Workers stay
  pinned to their captured profile.
  Verify Team On authorizes substantive in-scope delegation by default and
  balance `1` favors Worker handoff without weakening explicit task-specific
  delegation restrictions or Worker completeness, required checks, approvals,
  scope, concurrency, and depth limits.
- Verify collab spawn and `SubAgentActivity` start/completion edges admit direct
  and nested Worker activity before persisted metadata, preserve active lineages
  through delayed `ThreadStarted` delivery, and prune omitted idle/absent edges
  after the 30-second grace window without adding polling.
- Verify `[team.lead].balance` defaults to `3`, accepts only `1..5`, rejects
  Worker updates, and persists through session snapshots, resume, and fork.
  Verify `/team balance` renders all five approved labels and typed values,
  preserves Team Off and global config, keeps level `3` byte-for-byte at the
  current behavior, and changes only discretionary Lead oversight guidance;
  Worker scope, required checks, approvals, and configured efforts remain
  unchanged. Verify balance remains independent of `dynamic_handoff`, leaves
  `oversight_timeout_minutes` unchanged, and adds no polling loop.
- Verify `[team.lead].dynamic_handoff` defaults to `false`, is accepted under
  `[team.lead]`, injects bounded role-specific Lead/Worker guidance when true,
  keeps small or Lead-context-heavy lookups direct, requests concise selected
  evidence with pointers and uncertainty, and avoids routine duplicate
  lookups. Verify the setting persists through resume/fork snapshots, legacy
  snapshots default to `false`, and existing delegation authorization,
  concurrency, depth, and tool behavior remain unchanged.
- Verify optional `[team.worker].max_concurrent` accepts only positive values,
  is rejected under `[team.lead]`, atomically limits pending starts and active
  followups across both backends, releases on completion/abort/shutdown, leaves
  grandchildren outside the count, and remains additional to existing global
  agent-count, depth, and resource limits. Verify that it does not raise or
  replace those limits, and tells the Lead the value is a ceiling rather than a
  target.
- Verify `[team.lead].oversight_timeout_minutes` defaults to 30 minutes,
  accepts only `1..15768000`, and rejects out-of-range values; Lead idle
  parking makes no inference or polling on routine progress, retains only the
  bounded summary, wakes on standalone `send_message_action`, handoff,
  completion, escalation/failure, or user input, and emits one deadline wake
  without progress-based
  extension. Verify explicit `wait_agent` uses the same interval, a second
  interval arms only after a Lead assessment, cancellation,
  resume-without-workers, no-worker wait termination, and visible
  idle/deadline state. Verify Team Off serializes the final automatic-turn
  admission boundary, drops stale trigger mail while retaining queue-only
  communication, and permits already-admitted in-flight turns to finish.
- Verify automatic Goal continuations share the Lead admission boundary, remain
  parked while direct Workers are active, and recheck after Worker completion or
  actionable input without introducing a polling turn.
- Verify ordinary Goal external-wait parking attributes only exact managed
  unified-exec process IDs to the active Goal turn, wakes once on terminal or
  released processes, deduplicates concurrent continuations, and uses a
  monotonic intent generation to ignore stale terminal errors after same-ID
  objective edits or Goal replacement. Turn/Goal replacement, cancellation,
  and user input invalidate stale waiters while leaving external processes
  alive.
- Verify selected integer wait arguments accept only exactly integral decimal or
  exponent spellings from preserved raw lexemes, while nested `Value` fields,
  fractional/unsafe/overflow values, and existing handler bounds remain
  unchanged.
- Verify the upstream Goal empty-response breaker (PR #44320) counts only
  host-admitted automatic Goal turns, blocks after three consecutive empty
  final responses, resets on non-commentary activity/user input/objective or
  error changes, and preserves tracked external wait parking and wake behavior.
- Verify the upstream explicit pause semantics (PR #44290): `update_goal`
  accepts `paused` only for an explicit user request, accounts final progress
  with budget-limited precedence, and rejects resume and system-limit statuses.
- Verify a dependency-free Team Worker `wait_agent` queues one bounded handoff
  to its immediate parent without marking completion, suppresses repeated
  waits in one turn, rearms after meaningful parent input or new work, queues
  while the parent is paused, and leaves routine progress quiet. Verify the
  V2 wait first quiesces currently admitted model/tool operations and pending
  non-wait sibling dispatches, rechecks approval, user-input, usage, mailbox,
  and child state, and drops the signal when cancellation or another wait
  outcome wins. Its limitation remains
  dependencies visible at the boundary; work admitted afterward and sibling
  waits are not inferred. Legacy V1 explicit-target waits remain result
  collection and do not emit this signal.
- Verify `[team.lead].show_idle_notifications` defaults to false, suppresses
  passive idle and parked-wait notices without suppressing oversight deadline
  warnings or actionable wakes, and emits the passive notices when enabled.
  Verify legacy and resumed thread snapshots continue to follow the current
  global config value.
- Verify the reserved `collaboration.send_message` schema remains unchanged
  (target/message only, with its pre-team description), while Team mode exposes
  standalone `send_message_action` and routes it to an immediate Lead wake.
- Verify recursive usage accounting sends context counters and the complete
  projection through separate notifications, deduplicates each source response
  exactly once, includes cold-resumed and archived descendants plus forwarded
  Review usage, preserves parent/fork ownership and direct context totals, and
  leaves legacy aggregate daily history unchanged. Verify exact live responses
  remain visible as additive partial usage while a projection is pending or
  unavailable, then merge into the complete baseline without double counting;
  a complete zero baseline must remain distinct from an unavailable read, and
  transient startup reads recover through bounded retries without rescanning on
  every response.
- Verify root Fast/service-tier changes update loaded direct and nested
  ThreadSpawn settings snapshots and notifications, while already captured
  in-flight turns retain their request tier and later/new turns use the root
  selection.
- Verify V2 team admission accepts V1 Worker metadata, root startup/resume
  warns and disables only the affected thread when an assignment is invalid,
  restores saved model/effort unless explicit resume overrides are supplied,
  and keeps delegated Worker startup and live toggles strict. Verify that a V1
  Worker under a V2 Lead can spawn nested Workers when its metadata supports
  delegation while disabled assignments remain collaboration-tool-free.
- Verify decision-provenance matches remain advisory retrieval candidates: they
  never gate normal model flow, infer approval, or turn historical options into
  current approval choices.
- Verify `/decisions crossroads`, `/decisions show`, and `/decisions history`
  traverse candidate sources, linked records, and append-only review history;
  reviewed, dismissed, and revisited states remain bookkeeping only, including
  repeated review cycles.
- Verify short IDs are literal, case-sensitive, full-store unique lookups and
  mixed decision/crossroad matches require disambiguation rather than silently
  selecting a crossroad.
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
  model-visible inventory even if tool listing is temporarily unavailable;
  confirm the cached normal tool-plan path retains recovered placeholders.
- Verify the fork-preserved `update_plan` default remains enabled when omitted
  or given an empty table, while explicit `enabled = false` removes the tool
  from both registered and model-visible sets.
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
- Verify per-thread `usagePolicy` remains disabled by default, persists through
  resume and all fork modes, exposes bounded provider-window status to models,
  and only auto-resumes resettable provider limits while respecting the
  configured continuation floor. Verify raw response-item injections reopen an
  active turn after a final answer without being treated as explicit-user
  authorization that bypasses the continuation floor. Verify the TUI default
  and interval bounds,
  known-reset scheduling, hourly fallback account refresh, floor-paused work,
  `/continue` wake/report behavior, and cancellation/manual-stop preservation.
- Verify `/pause` and `/continue` affect only the selected Lead tree, reconcile
  newly loaded descendants, gate future model/tool starts and automatic Lead or
  usage wakes, preserve retained work without synthetic turns, and report
  running/pausing/paused separately from idle/working/waiting activity. Confirm
  already-launched external commands are neither suspended nor replayed and
  process-local activity is reconstructed through `thread/activity/read` after
  reconnect rather than cold-resume persistence. Verify the TUI uses the
  event-driven `Lead: idle|working|waiting · Workers: N working[, M waiting]`
  row, counts unfinished direct and nested Workers only within the selected
  root, keeps the title aligned with that projection, animates only actual
  work (and in-flight Pausing), honors reduced-motion settings, and renders
  the static `Paused · Lead + N workers · /continue to resume` row. Verify
  cancellation before activity or parallel-dispatch admission returns one
  normal aborted tool response without surfacing an internal `TurnAborted`
  fatal error, including the ready/ready arbitration boundary; verify a
  terminal completion claimed before cancellation is preserved and genuine
  dispatch join failures still surface.
- Verify the TUI sets its waiting header before interrupt-hint updates, and
  leaves an empty server collaboration-mode catalog empty while retaining only
  visible server-provided modes.
  Verify the running two-row Team/Workers/Subagents layout, direct Worker cap
  denominator, nested parent metadata hydration, and 30-second ordinary-wait
  grace: repeated waits must not extend it, expiry must redraw without a new
  event or animation, and approval/user-input/usage-limit/error/completion,
  close, and pause states must stay immediate. Verify terminal completion wins
  over delayed activity until the next `turn/started`, metadata-only root
  removal prunes descendants, reset/reconnect rebuilds only the selected loaded
  tree, and ephemeral/temporary helper threads do not enter the projection.
  Verify collab spawn and V2 `SubAgentActivity` start/completion events admit
  parent edges before metadata refresh, same-root refreshes retain a provisional
  edge for a bounded grace window, and later omissions prune stale/unloaded
  edges.
  Verify `codex-mcp-server`
  handles `ThreadActivityUpdated` exhaustively, forwards the notification, and
  continues waiting for real turn completion.
