//! Mode-scoped capability visibility configuration shared across crates.

use std::collections::HashMap;

use codex_protocol::config_types::ModeKind;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnablementFilterMode {
    #[default]
    Include,
    Exclude,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EnablementFilterConfig {
    #[serde(default)]
    pub mode: EnablementFilterMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModeEnablementConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<EnablementFilterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcps: Option<EnablementFilterConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<EnablementFilterConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EnablementConfig {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub modes: HashMap<ModeKind, ModeEnablementConfig>,
}
