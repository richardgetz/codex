use std::collections::BTreeMap;

use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExecPolicyToml {
    /// Named command-policy rulesets that can be selected for a session.
    #[serde(default)]
    pub rulesets: BTreeMap<String, ExecPolicyRulesetToml>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExecPolicyRulesetToml {
    /// Whether this ruleset overlays the normal policy or replaces it.
    pub mode: ExecPolicyRulesetMode,

    /// `.rules` files that make up this ruleset.
    pub files: Vec<AbsolutePathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ExecPolicyRulesetMode {
    Overlay,
    Exclusive,
}
