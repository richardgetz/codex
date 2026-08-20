use super::MultiAgentRoleInstructions;
use codex_extension_api::ContextualUserFragment;
use codex_utils_output_truncation::approx_token_count;

#[test]
fn role_instructions_are_bounded_before_rendering() {
    let text = "role guidance ".repeat(20_000);

    let fragment = MultiAgentRoleInstructions::unmarked(text);

    assert!(approx_token_count(&fragment.render()) < 9_000);
}
