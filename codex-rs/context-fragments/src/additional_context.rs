use codex_utils_string::truncate_middle_with_token_budget;

use crate::ContextualUserFragment;

const MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS: usize = 1_000;
const ADDITIONAL_CONTEXT_END_MARKER_SUFFIX: &str = ">";
const ADDITIONAL_CONTEXT_START_MARKER_PREFIX: &str = "<external_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionalContextUserFragment {
    key: String,
    value: String,
}

impl AdditionalContextUserFragment {
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }
}

impl ContextualUserFragment for AdditionalContextUserFragment {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            ADDITIONAL_CONTEXT_START_MARKER_PREFIX,
            ADDITIONAL_CONTEXT_END_MARKER_SUFFIX,
        )
    }

    fn matches_text(text: &str) -> bool {
        let trimmed = text.trim();
        let Some(rest) = trimmed.strip_prefix(ADDITIONAL_CONTEXT_START_MARKER_PREFIX) else {
            return false;
        };
        let Some((key, value_and_close)) = rest.split_once(ADDITIONAL_CONTEXT_END_MARKER_SUFFIX)
        else {
            return false;
        };

        value_and_close.ends_with(&format!("</external_{key}>"))
    }

    fn body(&self) -> String {
        additional_context_body(&self.key, &self.value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdditionalContextDeveloperFragment {
    key: String,
    value: String,
}

impl AdditionalContextDeveloperFragment {
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }
}

impl ContextualUserFragment for AdditionalContextDeveloperFragment {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        additional_context_developer_body(&self.key, &self.value)
    }
}

fn additional_context_body(key: &str, value: &str) -> String {
    let key = sanitize_additional_context_key(key);
    let value = truncate_middle_with_token_budget(value, MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS).0;
    let value = escape_additional_context_value(&value);
    format!("{key}>{value}</external_{key}")
}

fn additional_context_developer_body(key: &str, value: &str) -> String {
    let key = sanitize_additional_context_key(key);
    let value = truncate_middle_with_token_budget(value, MAX_ADDITIONAL_CONTEXT_VALUE_TOKENS).0;
    let value = escape_additional_context_value(&value);
    format!("<{key}>{value}</{key}>")
}

fn sanitize_additional_context_key(key: &str) -> String {
    let sanitized = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "context".to_string()
    } else {
        sanitized
    }
}

fn escape_additional_context_value(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::additional_context_body;
    use super::additional_context_developer_body;
    use pretty_assertions::assert_eq;

    #[test]
    fn additional_context_sanitizes_keys_for_model_tags() {
        assert_eq!(
            additional_context_body("browser tab</external_bad", "value"),
            "browser_tab__external_bad>value</external_browser_tab__external_bad"
        );
        assert_eq!(
            additional_context_developer_body("", "value"),
            "<context>value</context>"
        );
        assert_eq!(
            additional_context_body("browser", "x</external_browser><other>"),
            "browser>x&lt;/external_browser&gt;&lt;other&gt;</external_browser"
        );
    }
}
