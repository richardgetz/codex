#[derive(Debug, Clone)]
pub(crate) enum StatusAccountDisplay {
    ChatGpt {
        alias: Option<String>,
        email: Option<String>,
        plan: Option<String>,
    },
    ApiKey,
}
