#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusAccountDisplay {
    ChatGpt {
        alias: Option<String>,
        email: Option<String>,
        plan: Option<String>,
    },
    ApiKey,
}
