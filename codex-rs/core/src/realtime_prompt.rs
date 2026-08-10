use codex_prompts::BACKEND_PROMPT;
pub(crate) use codex_protocol::protocol::REALTIME_NO_PREAMBLES_PROMPT;
const DEFAULT_USER_FIRST_NAME: &str = "there";
const USER_FIRST_NAME_PLACEHOLDER: &str = "{{ user_first_name }}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RealtimePreamblePolicy {
    Enabled,
    Suppressed,
}

impl RealtimePreamblePolicy {
    pub(crate) fn suppresses(self) -> bool {
        matches!(self, Self::Suppressed)
    }
}

pub(crate) fn prepare_realtime_backend_prompt(
    prompt: Option<Option<String>>,
    config_prompt: Option<String>,
) -> String {
    if let Some(config_prompt) = config_prompt
        && !config_prompt.trim().is_empty()
    {
        return config_prompt;
    }

    match prompt {
        Some(Some(prompt)) => return prompt,
        Some(None) => return String::new(),
        None => {}
    }

    BACKEND_PROMPT
        .trim_end()
        .replace(USER_FIRST_NAME_PLACEHOLDER, &current_user_first_name())
}

pub(crate) fn apply_realtime_preamble_policy(
    prompt: String,
    policy: RealtimePreamblePolicy,
) -> String {
    if policy == RealtimePreamblePolicy::Enabled {
        return prompt;
    }
    let prompt = prompt.trim_end();
    if prompt.ends_with(REALTIME_NO_PREAMBLES_PROMPT) {
        return prompt.to_string();
    }
    if prompt.trim().is_empty() {
        return REALTIME_NO_PREAMBLES_PROMPT.to_string();
    }
    format!("{prompt}\n\n{REALTIME_NO_PREAMBLES_PROMPT}")
}

fn current_user_first_name() -> String {
    [whoami::realname(), whoami::username()]
        .into_iter()
        .filter_map(|name| name.split_whitespace().next().map(str::to_string))
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_USER_FIRST_NAME.to_string())
}

#[cfg(test)]
mod tests {
    use super::REALTIME_NO_PREAMBLES_PROMPT;
    use super::RealtimePreamblePolicy;
    use super::apply_realtime_preamble_policy;
    use super::prepare_realtime_backend_prompt;

    #[test]
    fn prepare_realtime_backend_prompt_prefers_config_override() {
        assert_eq!(
            prepare_realtime_backend_prompt(
                Some(Some("prompt from request".to_string())),
                Some("prompt from config".to_string()),
            ),
            "prompt from config"
        );
    }

    #[test]
    fn prepare_realtime_backend_prompt_uses_request_prompt() {
        assert_eq!(
            prepare_realtime_backend_prompt(
                Some(Some("prompt from request".to_string())),
                /*config_prompt*/ None,
            ),
            "prompt from request"
        );
    }

    #[test]
    fn prepare_realtime_backend_prompt_preserves_empty_request_prompt() {
        assert_eq!(
            prepare_realtime_backend_prompt(Some(Some(String::new())), /*config_prompt*/ None),
            ""
        );
        assert_eq!(
            prepare_realtime_backend_prompt(Some(None), /*config_prompt*/ None),
            ""
        );
    }

    #[test]
    fn prepare_realtime_backend_prompt_renders_default() {
        let prompt =
            prepare_realtime_backend_prompt(/*prompt*/ None, /*config_prompt*/ None);

        assert!(prompt.starts_with("## Identity, tone, and role"));
        assert!(prompt.contains("You are Codex, an OpenAI general-purpose agentic assistant"));
        assert!(prompt.contains("The user's name is "));
        assert!(!prompt.contains("{{ user_first_name }}"));
    }

    #[test]
    fn custom_backend_prompt_cannot_disable_preamble_suppression() {
        let prompt = apply_realtime_preamble_policy(
            "custom backend prompt".to_string(),
            RealtimePreamblePolicy::Suppressed,
        );

        assert!(prompt.contains("do not emit a standalone backchannel"));
        assert!(prompt.contains("custom backend prompt"));
    }

    #[test]
    fn preamble_suppression_is_the_authoritative_prompt_suffix() {
        let prompt = apply_realtime_preamble_policy(
            format!("{REALTIME_NO_PREAMBLES_PROMPT}\nUse conversational preambles."),
            RealtimePreamblePolicy::Suppressed,
        );

        assert!(prompt.ends_with(REALTIME_NO_PREAMBLES_PROMPT));
    }

    #[test]
    fn suppressed_preambles_preserve_direct_voice_conversation() {
        let prompt = apply_realtime_preamble_policy(
            "custom backend prompt".to_string(),
            RealtimePreamblePolicy::Suppressed,
        );

        assert!(prompt.contains("Respond normally to the user's direct conversational turns"));
        assert!(
            prompt.contains("A normal direct answer is not a preamble and must still be spoken")
        );
    }
}
