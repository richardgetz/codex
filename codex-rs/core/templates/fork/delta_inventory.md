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
    `gpt-5.4-mini` + `low`
- Orchestrator memory defaults:
  - `[orchestrator_memory]`
  - `enabled = true`
  - `scope = "orchestrator"`
- Orchestrator memory maintenance:
  - Slash command: `/orchestrator-memory-forget <needle>`
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
- Orchestrator primary contact channel:
  - Config: `[orchestrator].primary_contact`
  - Startup override: `--primary-contact <mcp>` or `--primary-contact off`
  - Harness-only polling uses `check_messages_every_seconds`, default `900`,
    and only calls the model when a new user message is found.
- Built-in scratchpad fallback:
  - Namespace: `scratchpad`
  - Stores JSON scratchpads under `<codex_home>/scratchpad/entries` unless a
    tool call provides `state_home`.
  - `open_scratchpad` defaults `scratchpad_id` to the current thread/session id.
  - Supports active/archived lookup plus archive/unarchive.
- Post-compaction recovery:
  - Config: `[orchestrator].recover_scratchpad_after_compaction`
  - Default: `true`
  - In Orchestrator mode, live compaction events enqueue a scratchpad recovery
    instruction; replayed history does not.
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
  orchestrator memory.
- Verify `[enablement.modes.<mode>]` still filters `skills`, `mcps`, and
  `plugins` correctly.
- Verify Orchestrator child spawns still respect
  `[orchestrator].allowed_spawn_modes`.
- Verify explicitly enabled Orchestrator MCPs remain callable inline for
  communication/state workflows.
- Verify configured primary contact polling starts in Orchestrator mode and does
  not wake the model for empty status responses.
- Verify built-in `scratchpad` remains available in Orchestrator mode and
  `open_scratchpad` uses the thread id when no id is provided.
- Verify live compaction events trigger scratchpad recovery while replayed
  compaction history does not.
- Verify memory helper naming still shows `Memory [extractor]` and
  `Memory [memory builder]`.
