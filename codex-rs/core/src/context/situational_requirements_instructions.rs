use super::ContextualUserFragment;
use codex_config::types::SituationalRequirementActionConfig;
use codex_config::types::SituationalRequirementRuleConfig;
use codex_config::types::SituationalRequirementsConfig;

const SITUATIONAL_REQUIREMENTS_OPEN_TAG: &str = "<situational_requirements>";
const SITUATIONAL_REQUIREMENTS_CLOSE_TAG: &str = "</situational_requirements>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SituationalRequirementsInstructions {
    rules: Vec<SituationalRequirementRuleConfig>,
}

impl SituationalRequirementsInstructions {
    pub(crate) fn new(config: &SituationalRequirementsConfig) -> Self {
        Self {
            rules: config.rules.clone(),
        }
    }
}

impl ContextualUserFragment for SituationalRequirementsInstructions {
    const ROLE: &'static str = "developer";
    const START_MARKER: &'static str = SITUATIONAL_REQUIREMENTS_OPEN_TAG;
    const END_MARKER: &'static str = SITUATIONAL_REQUIREMENTS_CLOSE_TAG;

    fn body(&self) -> String {
        let mut body = "\n## Situational Requirements\n\
The user enabled deterministic trigger/action requirements. When a trigger applies, complete the listed action before finalizing unless it is impossible; if impossible, move the blocked item or wait into the scratchpad and explain the blocker.\n\
Rules:\n"
            .to_string();
        for rule in &self.rules {
            body.push_str("- `");
            body.push_str(&rule.trigger.to_string());
            body.push_str("` -> ");
            let actions = rule
                .actions
                .iter()
                .map(render_action)
                .collect::<Vec<_>>()
                .join(", ");
            body.push_str(&actions);
            body.push('\n');
        }
        body
    }
}

fn render_action(action: &SituationalRequirementActionConfig) -> String {
    let mut rendered = format!("`{}`", action.action);
    if let Some(skill) = &action.skill {
        rendered.push_str(&format!(" skill `{skill}`"));
    }
    if let Some(mcp) = &action.mcp {
        rendered.push_str(&format!(" MCP `{mcp}`"));
    }
    if let Some(reason) = &action.reason {
        rendered.push_str(&format!(" ({reason})"));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_config::types::SituationalRequirementAction;
    use codex_config::types::SituationalRequirementActionConfig;
    use codex_config::types::SituationalRequirementRuleConfig;
    use codex_config::types::SituationalRequirementTrigger;

    #[test]
    fn body_renders_trigger_actions() {
        let config = SituationalRequirementsConfig {
            enabled: true,
            rules: vec![SituationalRequirementRuleConfig {
                trigger: SituationalRequirementTrigger::CodeChange,
                actions: vec![SituationalRequirementActionConfig {
                    action: SituationalRequirementAction::GitIntentNote,
                    mcp: Some("git-intent-notes".to_string()),
                    skill: None,
                    reason: Some("preserve intent".to_string()),
                }],
            }],
        };

        let body = SituationalRequirementsInstructions::new(&config).body();

        assert!(body.contains("`code_change` -> `git_intent_note`"));
        assert!(body.contains("MCP `git-intent-notes`"));
        assert!(body.contains("preserve intent"));
    }
}
