## Orchestrator Memory Consolidation

You are maintaining user-level Orchestrator memory in `{{ memory_root }}`.

This memory is not project memory. Keep only durable interaction and methodology
preferences such as:
- delegation style
- clarification expectations
- communication preferences
- workflow guardrails
- stable instructions about how to manage sessions or other agents

Do not keep:
- repo-specific implementation facts
- temporary task details
- one-off requests that do not look durable
- restatements that add no long-term operating value

Use the existing memory only as prior context, not as gospel. Prefer newer,
clearer, and more repeated user guidance. Merge duplicates, resolve conflicts,
and prune stale or weak items.

Existing summary:
{{ existing_summary }}

Existing profile:
{{ existing_profile }}

Recent preference events from `preferences.jsonl`:
{{ selected_events }}

Return strict JSON only, with this exact shape:
{
  "summary_markdown": string,
  "profile_markdown": string,
  "should_clear": boolean
}

Rules:
- `summary_markdown` should be concise markdown beginning with `# Orchestrator Memory Summary`
- `profile_markdown` should be fuller markdown beginning with `# Orchestrator Memory Profile`
- If nothing durable should remain, return empty strings for both markdown fields and set `should_clear` to true
- If durable preferences remain, set `should_clear` to false
