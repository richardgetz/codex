use super::PreviousSectionState;
use super::WorldStateSection;
use crate::context::ContextualUserFragment;
use crate::context::RealtimeEndInstructions;
use crate::context::RealtimeStartInstructions;
use crate::context::RealtimeStartWithInstructions;
use crate::realtime_context::truncate_realtime_text_to_token_budget;
use crate::realtime_prompt::REALTIME_NO_PREAMBLES_PROMPT;
use codex_prompts::START_INSTRUCTIONS;
use serde::Deserialize;
use serde::Serialize;

const REALTIME_PREAMBLES_ENABLED_PROMPT: &str = "Conversational backchannels and progress preambles are enabled for this live voice session. This current instruction supersedes any earlier realtime instruction to suppress them.";
const REALTIME_START_INSTRUCTIONS_TOKEN_BUDGET: usize = 8_000;

/// The realtime conversation state currently visible to the model.
#[derive(Clone, Debug)]
pub(crate) struct RealtimeState {
    snapshot: RealtimeSnapshot,
    start_instructions: Option<String>,
    end_instructions: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct RealtimeSnapshot {
    active: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    suppress_preambles: bool,
}

impl RealtimeState {
    pub(crate) fn new(
        active: bool,
        start_instructions: Option<&str>,
        end_instructions: Option<&str>,
    ) -> Self {
        Self {
            snapshot: RealtimeSnapshot {
                active,
                suppress_preambles: false,
            },
            start_instructions: start_instructions.map(str::to_string),
            end_instructions: end_instructions.map(str::to_string),
        }
    }

    pub(crate) fn suppress_preambles(mut self) -> Self {
        self.snapshot.suppress_preambles = true;
        self
    }

    fn bounded_start_instructions(&self) -> String {
        let instructions = self
            .start_instructions
            .as_deref()
            .unwrap_or_else(|| START_INSTRUCTIONS.trim());
        truncate_realtime_text_to_token_budget(
            instructions,
            REALTIME_START_INSTRUCTIONS_TOKEN_BUDGET,
        )
    }

    fn render_start(&self) -> Box<dyn ContextualUserFragment> {
        if self.snapshot.suppress_preambles {
            let instructions = self.bounded_start_instructions();
            return Box::new(RealtimeStartWithInstructions::new(format!(
                "{instructions}\n\n{REALTIME_NO_PREAMBLES_PROMPT}"
            )));
        }
        match self.start_instructions.as_deref() {
            Some(_) => Box::new(RealtimeStartWithInstructions::new(
                self.bounded_start_instructions(),
            )),
            None => Box::new(RealtimeStartInstructions),
        }
    }

    fn render_transition(
        &self,
        previous: &RealtimeSnapshot,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        match (previous.active, self.snapshot.active) {
            (false, true) => Some(self.render_start()),
            (true, false) => Some(match self.end_instructions.as_deref() {
                Some(instructions) => {
                    Box::new(RealtimeEndInstructions::with_instructions(instructions))
                }
                None => Box::new(RealtimeEndInstructions::with_reason("inactive")),
            }),
            (true, true) if previous.suppress_preambles != self.snapshot.suppress_preambles => {
                if self.snapshot.suppress_preambles {
                    Some(self.render_start())
                } else {
                    let instructions = self.bounded_start_instructions();
                    Some(Box::new(RealtimeStartWithInstructions::new(format!(
                        "{instructions}\n\n{REALTIME_PREAMBLES_ENABLED_PROMPT}"
                    ))))
                }
            }
            (false, false) | (true, true) => None,
        }
    }
}

impl WorldStateSection for RealtimeState {
    const ID: &'static str = "realtime";
    type Snapshot = RealtimeSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn matches_legacy_fragment(role: &str, text: &str) -> bool {
        role == "developer" && RealtimeStartInstructions::matches_text(text)
    }

    fn has_retained_fragment_matcher() -> bool {
        true
    }

    fn matches_retained_fragment(role: &str, text: &str) -> bool {
        Self::matches_legacy_fragment(role, text)
    }

    fn render_diff(
        &self,
        previous: PreviousSectionState<'_, Self::Snapshot>,
    ) -> Option<Box<dyn ContextualUserFragment>> {
        match previous {
            PreviousSectionState::Known(previous) if previous == &self.snapshot => None,
            PreviousSectionState::Known(previous) => self.render_transition(previous),
            PreviousSectionState::Absent | PreviousSectionState::Unknown
                if self.snapshot.active =>
            {
                Some(self.render_start())
            }
            PreviousSectionState::Absent | PreviousSectionState::Unknown => None,
        }
    }
}

#[cfg(test)]
#[path = "realtime_tests.rs"]
mod tests;
