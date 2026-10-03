use super::MAX_ENVIRONMENT_SUBAGENT_BYTES;
use super::MAX_ENVIRONMENT_SUBAGENTS;
use super::bounded_environment_context_subagents;

fn rendered_environment_context_bytes(lines: &str) -> usize {
    "  <subagents>\n  </subagents>\n".len()
        + lines
            .lines()
            .map(|line| "    \n".len() + line.len())
            .sum::<usize>()
}

#[test]
fn environment_context_subagents_share_entry_and_rendered_byte_bounds() {
    let entry_limited = bounded_environment_context_subagents(
        (0..MAX_ENVIRONMENT_SUBAGENTS + 2).map(|index| format!("<agent name=\"{index}\" />")),
    );
    assert_eq!(entry_limited.lines().count(), MAX_ENVIRONMENT_SUBAGENTS);
    assert!(rendered_environment_context_bytes(&entry_limited) <= MAX_ENVIRONMENT_SUBAGENT_BYTES);

    let byte_limited = bounded_environment_context_subagents(
        (0..MAX_ENVIRONMENT_SUBAGENTS + 2).map(|index| format!("{index}{}", "é".repeat(100))),
    );
    assert!(byte_limited.lines().count() < MAX_ENVIRONMENT_SUBAGENTS);
    assert!(rendered_environment_context_bytes(&byte_limited) <= MAX_ENVIRONMENT_SUBAGENT_BYTES);
}
