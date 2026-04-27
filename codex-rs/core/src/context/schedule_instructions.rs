use super::ContextualUserFragment;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScheduleInstructions;

impl ScheduleInstructions {
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl ContextualUserFragment for ScheduleInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = "<schedule_instructions>";
    const END_MARKER: &'static str = "</schedule_instructions>";

    fn body(&self) -> String {
        "\n## Built-in Schedule\n\nThe built-in `schedule` tool namespace is available in this mode for durable time-based or conditional future triggers.\n\
Use it when the user explicitly asks to be reminded later, describes a recurring routine, or asks you to check a condition at a future time. Also use it when a task has a future time obligation that should survive compaction, resume, or handoff.\n\
Do not use it for ordinary short waits inside the current turn; use existing wait tools or scratchpad pending waits for those.\n\
Prefer linking scheduled triggers to the built-in scratchpad with `scratchpad.scratchpad_id` when the trigger belongs to an active objective. Link to orchestrator memory with `orchestrator_memory` when the trigger captures a durable user pattern, preference, or recurring life/work routine.\n\
For conditional triggers, store the future condition and any relevant tool/MCP hints in `condition` so the future agent knows what to check before contacting the user.\n".to_string()
    }
}
