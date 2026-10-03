use super::GuardianConversationHistory;
use super::MAX_CUSTOM_PROMPT_BYTES;
use super::OVERSIZED_PROMPT_NOTICE;
use crate::context::ContextualUserFragment;
use pretty_assertions::assert_eq;

#[test]
fn configured_prompt_is_preserved_within_limit() {
    let prompt = "retrieve only relevant history";
    assert_eq!(
        GuardianConversationHistory {
            prompt: Some(prompt),
        }
        .body(),
        prompt
    );
}

#[test]
fn oversized_configured_prompt_uses_bounded_builtin_instructions() {
    let prompt = "x".repeat(MAX_CUSTOM_PROMPT_BYTES + 1);
    let body = GuardianConversationHistory {
        prompt: Some(&prompt),
    }
    .body();

    let built_in_body = GuardianConversationHistory { prompt: None }.body();
    assert_eq!(body, format!("{OVERSIZED_PROMPT_NOTICE}\n{built_in_body}"));
    assert!(body.len() <= MAX_CUSTOM_PROMPT_BYTES);
}
