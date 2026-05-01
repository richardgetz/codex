//! Types used to define loaded and effective Codex configuration values.

// Note this file should generally be restricted to simple struct/enum
// definitions that do not contain business logic.

pub use crate::mcp_types::AppToolApproval;
pub use crate::mcp_types::McpServerConfig;
pub use crate::mcp_types::McpServerDisabledReason;
pub use crate::mcp_types::McpServerEnvVar;
pub use crate::mcp_types::McpServerToolConfig;
pub use crate::mcp_types::McpServerTransportConfig;
pub use crate::mcp_types::RawMcpServerConfig;
pub use codex_protocol::config_types::AltScreenMode;
pub use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::EnvironmentVariablePattern;
pub use codex_protocol::config_types::ModeKind;
pub use codex_protocol::config_types::Personality;
pub use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::ShellEnvironmentPolicyInherit;
pub use codex_protocol::config_types::WebSearchMode;
use codex_protocol::openai_models::ReasoningEffort;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

pub use crate::tui_keymap::KeybindingSpec;
pub use crate::tui_keymap::KeybindingsSpec;
pub use crate::tui_keymap::TuiApprovalKeymap;
pub use crate::tui_keymap::TuiChatKeymap;
pub use crate::tui_keymap::TuiComposerKeymap;
pub use crate::tui_keymap::TuiEditorKeymap;
pub use crate::tui_keymap::TuiGlobalKeymap;
pub use crate::tui_keymap::TuiKeymap;
pub use crate::tui_keymap::TuiListKeymap;
pub use crate::tui_keymap::TuiPagerKeymap;

pub const DEFAULT_OTEL_ENVIRONMENT: &str = "dev";
pub const DEFAULT_MEMORIES_MAX_ROLLOUTS_PER_STARTUP: usize = 2;
pub const DEFAULT_MEMORIES_MAX_ROLLOUT_AGE_DAYS: i64 = 10;
pub const DEFAULT_MEMORIES_MIN_ROLLOUT_IDLE_HOURS: i64 = 6;
pub const DEFAULT_MEMORIES_MIN_RATE_LIMIT_REMAINING_PERCENT: i64 = 25;
pub const DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION: usize = 256;
pub const DEFAULT_MEMORIES_MAX_UNUSED_DAYS: i64 = 30;
pub const DEFAULT_RESUME_LOAD_TIMEOUT_SECONDS: u64 = 60;
pub const DEFAULT_RESUME_VISIBLE_TURN_LIMIT: usize = 80;
const MIN_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION: usize = 1;
const MAX_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION: usize = 4096;
const MIN_MEMORIES_MAX_ROLLOUTS_PER_STARTUP: usize = 1;
const MAX_MEMORIES_MAX_ROLLOUTS_PER_STARTUP: usize = 128;

const fn default_enabled() -> bool {
    true
}

fn default_resume_load_timeout_seconds() -> u64 {
    DEFAULT_RESUME_LOAD_TIMEOUT_SECONDS
}

fn default_resume_visible_turn_limit() -> usize {
    DEFAULT_RESUME_VISIBLE_TURN_LIMIT
}

/// Strategy used when reconstructing a session from a rollout file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResumeStrategy {
    /// Prefer the latest replacement-history compaction checkpoint plus the
    /// surviving rollout tail, falling back to full replay when no safe
    /// checkpoint exists.
    #[default]
    LatestCompaction,
    /// Always replay the full rollout file.
    Full,
}

/// Settings loaded from config.toml for session resume behavior.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ResumeToml {
    pub strategy: Option<ResumeStrategy>,
    #[serde(default = "default_resume_visible_turn_limit")]
    pub visible_turn_limit: usize,
    #[serde(default = "default_enabled")]
    pub lazy_hydrate_history: bool,
    #[serde(default = "default_resume_load_timeout_seconds")]
    pub load_timeout_seconds: u64,
    #[serde(default = "default_enabled")]
    pub inject_scratchpad: bool,
}

impl Default for ResumeToml {
    fn default() -> Self {
        Self {
            strategy: None,
            visible_turn_limit: DEFAULT_RESUME_VISIBLE_TURN_LIMIT,
            lazy_hydrate_history: true,
            load_timeout_seconds: DEFAULT_RESUME_LOAD_TIMEOUT_SECONDS,
            inject_scratchpad: true,
        }
    }
}

/// Effective resume settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeConfig {
    pub strategy: ResumeStrategy,
    pub visible_turn_limit: usize,
    pub lazy_hydrate_history: bool,
    pub load_timeout_seconds: u64,
    pub inject_scratchpad: bool,
}

impl Default for ResumeConfig {
    fn default() -> Self {
        let toml = ResumeToml::default();
        toml.into()
    }
}

impl From<ResumeToml> for ResumeConfig {
    fn from(toml: ResumeToml) -> Self {
        Self {
            strategy: toml.strategy.unwrap_or_default(),
            visible_turn_limit: toml.visible_turn_limit.max(1),
            lazy_hydrate_history: toml.lazy_hydrate_history,
            load_timeout_seconds: toml.load_timeout_seconds.max(1),
            inject_scratchpad: toml.inject_scratchpad,
        }
    }
}

/// Determine where Codex should store CLI auth credentials.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AuthCredentialsStoreMode {
    #[default]
    /// Persist credentials in CODEX_HOME/auth.json.
    File,
    /// Persist credentials in the keyring. Fail if unavailable.
    Keyring,
    /// Use keyring when available; otherwise, fall back to a file in CODEX_HOME.
    Auto,
    /// Store credentials in memory only for the current process.
    Ephemeral,
}

/// Determine where Codex should store and read MCP credentials.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OAuthCredentialsStoreMode {
    /// `Keyring` when available; otherwise, `File`.
    /// Credentials stored in the keyring will only be readable by Codex unless the user explicitly grants access via OS-level keyring access.
    #[default]
    Auto,
    /// CODEX_HOME/.credentials.json
    /// This file will be readable to Codex and other applications running as the same user.
    File,
    /// Keyring when available, otherwise fail.
    Keyring,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WindowsSandboxModeToml {
    Elevated,
    Unelevated,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct WindowsToml {
    pub sandbox: Option<WindowsSandboxModeToml>,
    /// Defaults to `true`. Set to `false` to launch the final sandboxed child
    /// process on `Winsta0\\Default` instead of a private desktop.
    pub sandbox_private_desktop: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, JsonSchema)]
pub enum UriBasedFileOpener {
    #[serde(rename = "vscode")]
    VsCode,

    #[serde(rename = "vscode-insiders")]
    VsCodeInsiders,

    #[serde(rename = "windsurf")]
    Windsurf,

    #[serde(rename = "cursor")]
    Cursor,

    /// Option to disable the URI-based file opener.
    #[serde(rename = "none")]
    None,
}

impl UriBasedFileOpener {
    pub fn get_scheme(&self) -> Option<&str> {
        match self {
            UriBasedFileOpener::VsCode => Some("vscode"),
            UriBasedFileOpener::VsCodeInsiders => Some("vscode-insiders"),
            UriBasedFileOpener::Windsurf => Some("windsurf"),
            UriBasedFileOpener::Cursor => Some("cursor"),
            UriBasedFileOpener::None => None,
        }
    }
}

/// Settings that govern if and what will be written to `~/.codex/history.jsonl`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct History {
    /// If true, history entries will not be written to disk.
    pub persistence: HistoryPersistence,

    /// If set, the maximum size of the history file in bytes. The oldest entries
    /// are dropped once the file exceeds this limit.
    pub max_bytes: Option<usize>,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryPersistence {
    /// Save all history entries to disk.
    #[default]
    SaveAll,
    /// Do not write history to disk.
    None,
}

// ===== Analytics configuration =====

/// Analytics settings loaded from config.toml. Fields are optional so we can apply defaults.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AnalyticsConfigToml {
    /// When `false`, disables analytics across Codex product surfaces in this profile.
    pub enabled: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct FeedbackConfigToml {
    /// When `false`, disables the feedback flow across Codex product surfaces.
    pub enabled: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolSuggestDiscoverableType {
    Connector,
    Plugin,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolSuggestDiscoverable {
    #[serde(rename = "type")]
    pub kind: ToolSuggestDiscoverableType,
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolSuggestDisabledTool {
    #[serde(rename = "type")]
    pub kind: ToolSuggestDiscoverableType,
    pub id: String,
}

impl ToolSuggestDisabledTool {
    pub fn plugin(id: impl Into<String>) -> Self {
        Self {
            kind: ToolSuggestDiscoverableType::Plugin,
            id: id.into(),
        }
    }

    pub fn connector(id: impl Into<String>) -> Self {
        Self {
            kind: ToolSuggestDiscoverableType::Connector,
            id: id.into(),
        }
    }

    pub fn normalized(&self) -> Option<Self> {
        let id = self.id.trim();
        (!id.is_empty()).then(|| Self {
            kind: self.kind,
            id: id.to_string(),
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolSuggestConfig {
    #[serde(default)]
    pub discoverables: Vec<ToolSuggestDiscoverable>,
    #[serde(default)]
    pub disabled_tools: Vec<ToolSuggestDisabledTool>,
}

/// Memories settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MemoriesToml {
    /// When `true`, external context sources mark the thread `memory_mode` as `"polluted"`.
    #[serde(alias = "no_memories_if_mcp_or_web_search")]
    pub disable_on_external_context: Option<bool>,
    /// When `false`, newly created threads are stored with `memory_mode = "disabled"` in the state DB.
    pub generate_memories: Option<bool>,
    /// When `false`, skip injecting memory usage instructions into developer prompts.
    pub use_memories: Option<bool>,
    /// Which collaboration modes may read from and generate memories.
    pub scope: Option<MemoriesScope>,
    /// Maximum number of recent raw memories retained for global consolidation.
    #[schemars(range(min = 1, max = 4096))]
    pub max_raw_memories_for_consolidation: Option<usize>,
    /// Maximum number of days since a memory was last used before it becomes ineligible for phase 2 selection.
    pub max_unused_days: Option<i64>,
    /// Maximum age of the threads used for memories.
    pub max_rollout_age_days: Option<i64>,
    /// Maximum number of rollout candidates processed per pass.
    #[schemars(range(min = 1, max = 128))]
    pub max_rollouts_per_startup: Option<usize>,
    /// Minimum idle time between last thread activity and memory creation (hours). > 12h recommended.
    pub min_rollout_idle_hours: Option<i64>,
    /// Minimum remaining percentage required in Codex rate-limit windows before memory startup runs.
    #[schemars(range(min = 0, max = 100))]
    pub min_rate_limit_remaining_percent: Option<i64>,
    /// Model used for thread summarisation.
    pub extract_model: Option<String>,
    /// Model used for memory consolidation.
    pub consolidation_model: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemoriesScope {
    #[default]
    All,
    Orchestrator,
}

/// Effective memories settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoriesConfig {
    pub disable_on_external_context: bool,
    pub generate_memories: bool,
    pub use_memories: bool,
    pub scope: MemoriesScope,
    pub max_raw_memories_for_consolidation: usize,
    pub max_unused_days: i64,
    pub max_rollout_age_days: i64,
    pub max_rollouts_per_startup: usize,
    pub min_rollout_idle_hours: i64,
    pub min_rate_limit_remaining_percent: i64,
    pub extract_model: Option<String>,
    pub consolidation_model: Option<String>,
}

impl Default for MemoriesConfig {
    fn default() -> Self {
        Self {
            disable_on_external_context: false,
            generate_memories: true,
            use_memories: true,
            scope: MemoriesScope::All,
            max_raw_memories_for_consolidation: DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
            max_unused_days: DEFAULT_MEMORIES_MAX_UNUSED_DAYS,
            max_rollout_age_days: DEFAULT_MEMORIES_MAX_ROLLOUT_AGE_DAYS,
            max_rollouts_per_startup: DEFAULT_MEMORIES_MAX_ROLLOUTS_PER_STARTUP,
            min_rollout_idle_hours: DEFAULT_MEMORIES_MIN_ROLLOUT_IDLE_HOURS,
            min_rate_limit_remaining_percent: DEFAULT_MEMORIES_MIN_RATE_LIMIT_REMAINING_PERCENT,
            extract_model: None,
            consolidation_model: None,
        }
    }
}

impl From<MemoriesToml> for MemoriesConfig {
    fn from(toml: MemoriesToml) -> Self {
        let defaults = Self::default();
        Self {
            disable_on_external_context: toml
                .disable_on_external_context
                .unwrap_or(defaults.disable_on_external_context),
            generate_memories: toml.generate_memories.unwrap_or(defaults.generate_memories),
            use_memories: toml.use_memories.unwrap_or(defaults.use_memories),
            scope: toml.scope.unwrap_or(defaults.scope),
            max_raw_memories_for_consolidation: toml
                .max_raw_memories_for_consolidation
                .unwrap_or(defaults.max_raw_memories_for_consolidation)
                .clamp(
                    MIN_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
                    MAX_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION,
                ),
            max_unused_days: toml
                .max_unused_days
                .unwrap_or(defaults.max_unused_days)
                .clamp(0, 365),
            max_rollout_age_days: toml
                .max_rollout_age_days
                .unwrap_or(defaults.max_rollout_age_days)
                .clamp(0, 90),
            max_rollouts_per_startup: toml
                .max_rollouts_per_startup
                .unwrap_or(defaults.max_rollouts_per_startup)
                .clamp(
                    MIN_MEMORIES_MAX_ROLLOUTS_PER_STARTUP,
                    MAX_MEMORIES_MAX_ROLLOUTS_PER_STARTUP,
                ),
            min_rollout_idle_hours: toml
                .min_rollout_idle_hours
                .unwrap_or(defaults.min_rollout_idle_hours)
                .clamp(1, 48),
            min_rate_limit_remaining_percent: toml
                .min_rate_limit_remaining_percent
                .unwrap_or(defaults.min_rate_limit_remaining_percent)
                .clamp(0, 100),
            extract_model: toml.extract_model,
            consolidation_model: toml.consolidation_model,
        }
    }
}

/// Orchestrator-memory settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorMemoryToml {
    /// When `false`, skip injecting orchestrator-memory instructions into developer prompts.
    pub enabled: Option<bool>,
    /// Which collaboration modes may read orchestrator memory.
    pub scope: Option<MemoriesScope>,
    /// Seconds to wait after a newly observed preference before consolidating the summary.
    pub debounce_seconds: Option<u64>,
    /// Number of similar steering observations required before inferring a preference automatically.
    #[schemars(range(min = 1, max = 8))]
    pub min_observations: Option<usize>,
    /// Number of recent user turns to inspect when looking for repeated steering patterns.
    #[schemars(range(min = 1, max = 64))]
    pub recent_turn_window: Option<usize>,
    /// Maximum number of consolidated preference lines kept in the injected summary.
    #[schemars(range(min = 1, max = 128))]
    pub max_summary_items: Option<usize>,
    /// When true, run a model classifier even when heuristic extraction finds no memory signal.
    pub model_on_heuristic_miss: Option<bool>,
    /// When true, use a model agent to rewrite summary/profile artifacts after memory writes.
    pub model_consolidation: Option<bool>,
    /// Scheduled raw-event cleanup and deep mechanical consolidation.
    pub cleanup: Option<OrchestratorMemoryCleanupToml>,
}

/// Scheduled cleanup settings for orchestrator memory.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorMemoryCleanupToml {
    /// When false, skip scheduled orchestrator-memory cleanup.
    pub enabled: Option<bool>,
    /// Local HH:MM time when cleanup should run once per day.
    pub schedule: Option<String>,
    /// When true, a missed scheduled cleanup runs next time Codex starts.
    pub run_missed_on_startup: Option<bool>,
    /// When true, rewrite preferences.jsonl into a compact canonical event log.
    pub dedupe_raw_events: Option<bool>,
    /// When true, regenerate summary/profile artifacts after cleanup.
    pub deep_consolidation: Option<bool>,
    /// When true, run a memory-builder model pass to merge semantic near-duplicates.
    pub model_consolidation: Option<bool>,
    /// Days to keep forget tombstones before pruning them. Set 0 to drop them
    /// after the cleanup pass applies them.
    pub retain_forget_events_days: Option<u64>,
}

/// Built-in scratchpad behavior for one collaboration mode.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ScratchpadModeToml {
    /// When false, the built-in scratchpad tool namespace and scratchpad
    /// developer guidance are not exposed in this mode.
    pub enabled: Option<bool>,
    /// When false, live compaction events do not mechanically loop recovered
    /// scratchpad state back into the next model turn in this mode.
    pub recover_after_compaction: Option<bool>,
}

/// Built-in scratchpad settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ScratchpadToml {
    /// Global default for built-in scratchpad tool/guidance exposure.
    pub enabled: Option<bool>,
    /// Global default for live compaction recovery loopback.
    pub recover_after_compaction: Option<bool>,
    /// Archive non-archived scratchpads after this many days without updates.
    /// Set to 0 to disable automatic archiving.
    pub auto_archive_after_days: Option<u64>,
    /// Delete archived scratchpads after this many days in the archive.
    /// Set to 0 to disable automatic deletion.
    pub delete_archived_after_days: Option<u64>,
    /// When true, agents may proactively record measurable outcome datapoints.
    pub outcomes_enabled: Option<bool>,
    /// TUI rendering controls for live scratchpad update cards.
    pub view: Option<ScratchpadViewToml>,
    /// Collaboration-mode-specific overrides.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub modes: HashMap<ModeKind, ScratchpadModeToml>,
}

/// TUI rendering controls for live scratchpad update cards.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ScratchpadViewToml {
    /// When false, live scratchpad update cards are hidden in the TUI.
    pub enabled: Option<bool>,
    /// When false, the scratchpad id is omitted from the live card title.
    pub show_id: Option<bool>,
    /// Maximum completed items to show in live cards.
    pub completed_items: Option<usize>,
    /// Maximum next-step items to show in live cards.
    pub next_steps: Option<usize>,
    /// Maximum pending-wait items to show in live cards.
    pub pending_waits: Option<usize>,
}

/// Effective TUI rendering controls for live scratchpad update cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScratchpadViewConfig {
    pub enabled: bool,
    pub show_id: bool,
    pub completed_items: usize,
    pub next_steps: usize,
    pub pending_waits: usize,
}

impl Default for ScratchpadViewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_id: true,
            completed_items: 1,
            next_steps: 5,
            pending_waits: 5,
        }
    }
}

impl From<Option<ScratchpadViewToml>> for ScratchpadViewConfig {
    fn from(toml: Option<ScratchpadViewToml>) -> Self {
        let defaults = ScratchpadViewConfig::default();
        let Some(toml) = toml else {
            return defaults;
        };
        Self {
            enabled: toml.enabled.unwrap_or(defaults.enabled),
            show_id: toml.show_id.unwrap_or(defaults.show_id),
            completed_items: toml.completed_items.unwrap_or(defaults.completed_items),
            next_steps: toml.next_steps.unwrap_or(defaults.next_steps),
            pending_waits: toml.pending_waits.unwrap_or(defaults.pending_waits),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScratchpadModeConfig {
    pub enabled: bool,
    pub recover_after_compaction: bool,
}

impl ScratchpadModeConfig {
    const fn default_for_mode(mode: ModeKind) -> Self {
        match mode {
            ModeKind::Plan => Self {
                enabled: false,
                recover_after_compaction: false,
            },
            ModeKind::Default
            | ModeKind::Orchestrator
            | ModeKind::PairProgramming
            | ModeKind::Execute => Self {
                enabled: true,
                recover_after_compaction: true,
            },
        }
    }
}

/// Effective built-in scratchpad settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchpadConfig {
    pub modes: HashMap<ModeKind, ScratchpadModeConfig>,
    pub auto_archive_after_days: u64,
    pub delete_archived_after_days: u64,
    pub outcomes_enabled: bool,
    pub view: ScratchpadViewConfig,
}

impl Default for ScratchpadConfig {
    fn default() -> Self {
        let modes = [
            ModeKind::Default,
            ModeKind::Plan,
            ModeKind::Orchestrator,
            ModeKind::PairProgramming,
            ModeKind::Execute,
        ]
        .into_iter()
        .map(|mode| (mode, ScratchpadModeConfig::default_for_mode(mode)))
        .collect();
        Self {
            modes,
            auto_archive_after_days: 30,
            delete_archived_after_days: 90,
            outcomes_enabled: false,
            view: ScratchpadViewConfig::default(),
        }
    }
}

impl ScratchpadConfig {
    pub fn for_mode(&self, mode: ModeKind) -> ScratchpadModeConfig {
        self.modes
            .get(&mode)
            .copied()
            .unwrap_or_else(|| ScratchpadModeConfig::default_for_mode(mode))
    }
}

impl From<ScratchpadToml> for ScratchpadConfig {
    fn from(toml: ScratchpadToml) -> Self {
        let defaults = ScratchpadConfig::default();
        let default_auto_archive_after_days = defaults.auto_archive_after_days;
        let default_delete_archived_after_days = defaults.delete_archived_after_days;
        let modes = defaults
            .modes
            .into_iter()
            .map(|(mode, default)| {
                let mode_toml = toml.modes.get(&mode);
                (
                    mode,
                    ScratchpadModeConfig {
                        enabled: mode_toml
                            .and_then(|config| config.enabled)
                            .or(toml.enabled)
                            .unwrap_or(default.enabled),
                        recover_after_compaction: mode_toml
                            .and_then(|config| config.recover_after_compaction)
                            .or(toml.recover_after_compaction)
                            .unwrap_or(default.recover_after_compaction),
                    },
                )
            })
            .collect();
        Self {
            modes,
            auto_archive_after_days: toml
                .auto_archive_after_days
                .unwrap_or(default_auto_archive_after_days),
            delete_archived_after_days: toml
                .delete_archived_after_days
                .unwrap_or(default_delete_archived_after_days),
            outcomes_enabled: toml.outcomes_enabled.unwrap_or(defaults.outcomes_enabled),
            view: toml.view.into(),
        }
    }
}

/// Built-in schedule behavior for one collaboration mode.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ScheduleModeToml {
    /// When false, the built-in schedule tool namespace is not exposed in this mode.
    pub enabled: Option<bool>,
}

/// Built-in schedule settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ScheduleToml {
    /// Global default for built-in schedule tool exposure.
    pub enabled: Option<bool>,
    /// Collaboration-mode-specific overrides.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub modes: HashMap<ModeKind, ScheduleModeToml>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleModeConfig {
    pub enabled: bool,
}

impl ScheduleModeConfig {
    const fn default_for_mode(mode: ModeKind) -> Self {
        match mode {
            ModeKind::Orchestrator => Self { enabled: true },
            ModeKind::Default | ModeKind::Plan | ModeKind::PairProgramming | ModeKind::Execute => {
                Self { enabled: false }
            }
        }
    }
}

/// Effective built-in schedule settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleConfig {
    pub modes: HashMap<ModeKind, ScheduleModeConfig>,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        let modes = [
            ModeKind::Default,
            ModeKind::Plan,
            ModeKind::Orchestrator,
            ModeKind::PairProgramming,
            ModeKind::Execute,
        ]
        .into_iter()
        .map(|mode| (mode, ScheduleModeConfig::default_for_mode(mode)))
        .collect();
        Self { modes }
    }
}

impl ScheduleConfig {
    pub fn for_mode(&self, mode: ModeKind) -> ScheduleModeConfig {
        self.modes
            .get(&mode)
            .copied()
            .unwrap_or_else(|| ScheduleModeConfig::default_for_mode(mode))
    }
}

impl From<ScheduleToml> for ScheduleConfig {
    fn from(toml: ScheduleToml) -> Self {
        let defaults = ScheduleConfig::default();
        let modes = defaults
            .modes
            .into_iter()
            .map(|(mode, default)| {
                let mode_toml = toml.modes.get(&mode);
                (
                    mode,
                    ScheduleModeConfig {
                        enabled: mode_toml
                            .and_then(|config| config.enabled)
                            .or(toml.enabled)
                            .unwrap_or(default.enabled),
                    },
                )
            })
            .collect();
        Self { modes }
    }
}

/// Managed account-alias settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AccountsToml {
    /// Alias whose auth should be active by default for this Codex install.
    pub active: Option<String>,
    /// Ordered account aliases to use when automatic session-local account
    /// rotation is enabled. Use `"default"` for the original root auth store.
    #[serde(default)]
    pub rotation: Option<Vec<String>>,
}

/// Effective managed account-alias settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountsConfig {
    pub active: Option<String>,
    pub rotation: Vec<String>,
}

impl From<AccountsToml> for AccountsConfig {
    fn from(toml: AccountsToml) -> Self {
        Self {
            active: toml.active.and_then(|value| {
                let value = value.trim().to_string();
                (!value.is_empty()).then_some(value)
            }),
            rotation: normalize_account_rotation(toml.rotation.unwrap_or_default()),
        }
    }
}

fn normalize_account_rotation(rotation: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for alias in rotation {
        let alias = alias.trim();
        if alias.is_empty() {
            continue;
        }
        let alias = if alias.eq_ignore_ascii_case("default") {
            "default".to_string()
        } else {
            alias.to_string()
        };
        if !normalized
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&alias))
        {
            normalized.push(alias);
        }
    }
    normalized
}

/// Effective orchestrator-memory settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorMemoryConfig {
    pub enabled: bool,
    pub scope: MemoriesScope,
    pub debounce_seconds: u64,
    pub min_observations: usize,
    pub recent_turn_window: usize,
    pub max_summary_items: usize,
    pub model_on_heuristic_miss: bool,
    pub model_consolidation: bool,
    pub cleanup: OrchestratorMemoryCleanupConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorMemoryCleanupConfig {
    pub enabled: bool,
    pub schedule: String,
    pub run_missed_on_startup: bool,
    pub dedupe_raw_events: bool,
    pub deep_consolidation: bool,
    pub model_consolidation: bool,
    pub retain_forget_events_days: u64,
}

impl Default for OrchestratorMemoryCleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: "03:30".to_string(),
            run_missed_on_startup: true,
            dedupe_raw_events: true,
            deep_consolidation: true,
            model_consolidation: true,
            retain_forget_events_days: 30,
        }
    }
}

impl Default for OrchestratorMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scope: MemoriesScope::Orchestrator,
            debounce_seconds: 60,
            min_observations: 2,
            recent_turn_window: 8,
            max_summary_items: 24,
            model_on_heuristic_miss: false,
            model_consolidation: false,
            cleanup: OrchestratorMemoryCleanupConfig::default(),
        }
    }
}

impl From<OrchestratorMemoryToml> for OrchestratorMemoryConfig {
    fn from(toml: OrchestratorMemoryToml) -> Self {
        let defaults = Self::default();
        Self {
            enabled: toml.enabled.unwrap_or(defaults.enabled),
            scope: toml.scope.unwrap_or(defaults.scope),
            debounce_seconds: toml
                .debounce_seconds
                .unwrap_or(defaults.debounce_seconds)
                .clamp(5, 3600),
            min_observations: toml
                .min_observations
                .unwrap_or(defaults.min_observations)
                .clamp(1, 8),
            recent_turn_window: toml
                .recent_turn_window
                .unwrap_or(defaults.recent_turn_window)
                .clamp(1, 64),
            max_summary_items: toml
                .max_summary_items
                .unwrap_or(defaults.max_summary_items)
                .clamp(1, 128),
            model_on_heuristic_miss: toml
                .model_on_heuristic_miss
                .unwrap_or(defaults.model_on_heuristic_miss),
            model_consolidation: toml
                .model_consolidation
                .unwrap_or(defaults.model_consolidation),
            cleanup: toml.cleanup.unwrap_or_default().into(),
        }
    }
}

impl From<OrchestratorMemoryCleanupToml> for OrchestratorMemoryCleanupConfig {
    fn from(toml: OrchestratorMemoryCleanupToml) -> Self {
        let defaults = Self::default();
        let schedule = toml
            .schedule
            .map(|schedule| schedule.trim().to_string())
            .filter(|schedule| !schedule.is_empty())
            .unwrap_or(defaults.schedule);
        Self {
            enabled: toml.enabled.unwrap_or(defaults.enabled),
            schedule,
            run_missed_on_startup: toml
                .run_missed_on_startup
                .unwrap_or(defaults.run_missed_on_startup),
            dedupe_raw_events: toml.dedupe_raw_events.unwrap_or(defaults.dedupe_raw_events),
            deep_consolidation: toml
                .deep_consolidation
                .unwrap_or(defaults.deep_consolidation),
            model_consolidation: toml
                .model_consolidation
                .unwrap_or(defaults.model_consolidation),
            retain_forget_events_days: toml
                .retain_forget_events_days
                .unwrap_or(defaults.retain_forget_events_days)
                .min(365),
        }
    }
}

/// Orchestrator-specific thread-control settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorThreadControlToml {
    /// Model to use for orchestrator wake-up turns.
    pub model: Option<String>,
    /// Reasoning effort to use for orchestrator wake-up turns.
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// How Orchestrator mode should raise blockers that need user attention.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrchestratorEscalationMode {
    #[default]
    Inline,
    Mcp,
    Both,
}

/// Orchestrator escalation settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorEscalationToml {
    /// Where blockers that need the user should be raised.
    pub mode: Option<OrchestratorEscalationMode>,
    /// Optional MCP server or communication channel name to use when `mode`
    /// includes `mcp`.
    pub channel: Option<String>,
    /// Optional MCP tool name to use when `mode` includes `mcp`.
    pub tool: Option<String>,
}

/// Primary user-contact channel that Orchestrator mode should start on session boot.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorPrimaryContactToml {
    /// When `true`, Orchestrator starts the configured communication MCP at session boot.
    pub enabled: Option<bool>,
    /// MCP server or communication channel name, for example `"imessage"`.
    pub mcp: Option<String>,
    /// Optional MCP tool name that the harness invokes to arm the channel, for example
    /// `"imessage_followup_start"`.
    pub tool: Option<String>,
    /// Optional MCP tool name used by the harness-only background poller to
    /// check for new user messages.
    pub check_tool: Option<String>,
    /// How often the harness should check the primary contact channel for new
    /// user messages. Defaults to 900 seconds. `0` disables polling.
    pub check_messages_every_seconds: Option<u32>,
    /// Optional local-time schedule that overrides
    /// `check_messages_every_seconds` when a rule matches.
    pub schedule: Option<Vec<OrchestratorPrimaryContactScheduleToml>>,
    /// Deprecated: ignored. Primary-contact startup is harness-only and no
    /// longer injects a model-visible startup prompt.
    pub startup_prompt: Option<String>,
}

/// Local-time primary-contact polling interval override.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorPrimaryContactScheduleToml {
    /// Optional day names. Omit or leave empty to match every day.
    pub days: Option<Vec<String>>,
    /// Local start time in `HH:MM` 24-hour format.
    pub start: Option<String>,
    /// Local end time in `HH:MM` 24-hour format.
    pub end: Option<String>,
    /// Polling interval to use when this schedule entry matches.
    pub check_messages_every_seconds: Option<u32>,
}

/// Orchestrator behavior settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OrchestratorToml {
    #[serde(default)]
    pub escalation: Option<OrchestratorEscalationToml>,
    #[serde(default)]
    pub primary_contact: Option<OrchestratorPrimaryContactToml>,
    /// When true, Orchestrator mode schedules a scratchpad recovery prompt
    /// after a context compaction item is observed.
    pub recover_scratchpad_after_compaction: Option<bool>,
    /// How often Orchestrator mode should proactively re-check active workers
    /// when supervision state has not otherwise changed. `0` disables
    /// proactive model check-ins and relies only on mechanical state changes.
    pub active_agent_checkin_seconds: Option<u32>,
    /// Collaboration modes Orchestrator mode is allowed to launch for child
    /// agents. Defaults to `["default"]`.
    pub allowed_spawn_modes: Option<Vec<ModeKind>>,
}

/// Thread-control settings loaded from config.toml.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ThreadControlToml {
    #[serde(default)]
    pub orchestrator: Option<OrchestratorThreadControlToml>,
}

/// Effective orchestrator escalation settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorEscalationConfig {
    pub mode: OrchestratorEscalationMode,
    pub channel: Option<String>,
    pub tool: Option<String>,
}

impl Default for OrchestratorEscalationConfig {
    fn default() -> Self {
        Self {
            mode: OrchestratorEscalationMode::Inline,
            channel: None,
            tool: None,
        }
    }
}

impl From<OrchestratorEscalationToml> for OrchestratorEscalationConfig {
    fn from(toml: OrchestratorEscalationToml) -> Self {
        let defaults = Self::default();
        Self {
            mode: toml.mode.unwrap_or(defaults.mode),
            channel: toml.channel.and_then(|value| {
                let value = value.trim().to_string();
                (!value.is_empty()).then_some(value)
            }),
            tool: toml.tool.and_then(|value| {
                let value = value.trim().to_string();
                (!value.is_empty()).then_some(value)
            }),
        }
    }
}

/// Effective orchestrator behavior settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorConfig {
    pub escalation: OrchestratorEscalationConfig,
    pub primary_contact: OrchestratorPrimaryContactConfig,
    pub recover_scratchpad_after_compaction: bool,
    pub active_agent_checkin_seconds: u32,
    pub allowed_spawn_modes: Vec<ModeKind>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            escalation: OrchestratorEscalationConfig::default(),
            primary_contact: OrchestratorPrimaryContactConfig::default(),
            recover_scratchpad_after_compaction: true,
            active_agent_checkin_seconds: 600,
            allowed_spawn_modes: vec![ModeKind::Default],
        }
    }
}

impl From<OrchestratorToml> for OrchestratorConfig {
    fn from(toml: OrchestratorToml) -> Self {
        Self {
            escalation: toml.escalation.unwrap_or_default().into(),
            primary_contact: toml.primary_contact.unwrap_or_default().into(),
            recover_scratchpad_after_compaction: toml
                .recover_scratchpad_after_compaction
                .unwrap_or_else(|| Self::default().recover_scratchpad_after_compaction),
            active_agent_checkin_seconds: toml
                .active_agent_checkin_seconds
                .unwrap_or_else(|| Self::default().active_agent_checkin_seconds),
            allowed_spawn_modes: toml
                .allowed_spawn_modes
                .filter(|modes| !modes.is_empty())
                .unwrap_or_else(|| Self::default().allowed_spawn_modes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorPrimaryContactConfig {
    pub enabled: bool,
    pub mcp: Option<String>,
    pub tool: Option<String>,
    pub check_tool: Option<String>,
    pub check_messages_every_seconds: u32,
    pub schedule: Vec<OrchestratorPrimaryContactScheduleToml>,
    pub startup_prompt: Option<String>,
}

impl Default for OrchestratorPrimaryContactConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mcp: None,
            tool: None,
            check_tool: None,
            check_messages_every_seconds: 900,
            schedule: Vec::new(),
            startup_prompt: None,
        }
    }
}

impl From<OrchestratorPrimaryContactToml> for OrchestratorPrimaryContactConfig {
    fn from(toml: OrchestratorPrimaryContactToml) -> Self {
        let defaults = Self::default();
        Self {
            enabled: toml.enabled.unwrap_or(false),
            mcp: trimmed_non_empty(toml.mcp),
            tool: trimmed_non_empty(toml.tool),
            check_tool: trimmed_non_empty(toml.check_tool),
            check_messages_every_seconds: toml
                .check_messages_every_seconds
                .unwrap_or(defaults.check_messages_every_seconds),
            schedule: toml.schedule.unwrap_or_default(),
            startup_prompt: trimmed_non_empty(toml.startup_prompt),
        }
    }
}

fn trimmed_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    })
}

/// Effective orchestrator-specific thread-control settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OrchestratorThreadControlConfig {
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Effective thread-control settings after defaults are applied.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThreadControlConfig {
    pub orchestrator: OrchestratorThreadControlConfig,
}

impl From<ThreadControlToml> for ThreadControlConfig {
    fn from(toml: ThreadControlToml) -> Self {
        let orchestrator = toml.orchestrator.unwrap_or_default();
        Self {
            orchestrator: OrchestratorThreadControlConfig {
                model: orchestrator.model,
                reasoning_effort: orchestrator.reasoning_effort,
            },
        }
    }
}

/// Default settings that apply to all apps.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppsDefaultConfig {
    /// When `false`, apps are disabled unless overridden by per-app settings.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Whether tools with `destructive_hint = true` are allowed by default.
    #[serde(
        default = "default_enabled",
        skip_serializing_if = "std::clone::Clone::clone"
    )]
    pub destructive_enabled: bool,

    /// Whether tools with `open_world_hint = true` are allowed by default.
    #[serde(
        default = "default_enabled",
        skip_serializing_if = "std::clone::Clone::clone"
    )]
    pub open_world_enabled: bool,
}

/// Per-tool settings for a single app tool.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppToolConfig {
    /// Whether this tool is enabled. `Some(true)` explicitly allows this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    /// Approval mode for this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<AppToolApproval>,
}

/// Tool settings for a single app.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppToolsConfig {
    /// Per-tool overrides keyed by tool name (for example `repos/list`).
    #[serde(default, flatten)]
    pub tools: HashMap<String, AppToolConfig>,
}

/// Config values for a single app/connector.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppConfig {
    /// When `false`, Codex does not surface this app.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Whether tools with `destructive_hint = true` are allowed for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_enabled: Option<bool>,

    /// Whether tools with `open_world_hint = true` are allowed for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_enabled: Option<bool>,

    /// Approval mode for tools in this app unless a tool override exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<AppToolApproval>,

    /// Whether tools are enabled by default for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_enabled: Option<bool>,

    /// Per-tool settings for this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<AppToolsConfig>,
}

/// App/connector settings loaded from `config.toml`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct AppsConfigToml {
    /// Default settings for all apps.
    #[serde(default, rename = "_default", skip_serializing_if = "Option::is_none")]
    pub default: Option<AppsDefaultConfig>,

    /// Per-app settings keyed by app ID (for example `[apps.google_drive]`).
    #[serde(default, flatten)]
    pub apps: HashMap<String, AppConfig>,
}

// ===== OTEL configuration =====

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OtelHttpProtocol {
    /// Binary payload
    Binary,
    /// JSON payload
    Json,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub struct OtelTlsConfig {
    pub ca_certificate: Option<AbsolutePathBuf>,
    pub client_certificate: Option<AbsolutePathBuf>,
    pub client_private_key: Option<AbsolutePathBuf>,
}

/// Which OTEL exporter to use.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema)]
#[schemars(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
pub enum OtelExporterKind {
    None,
    Statsig,
    OtlpHttp {
        endpoint: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        protocol: OtelHttpProtocol,
        #[serde(default)]
        tls: Option<OtelTlsConfig>,
    },
    OtlpGrpc {
        endpoint: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        tls: Option<OtelTlsConfig>,
    },
}

/// OTEL settings loaded from config.toml. Fields are optional so we can apply defaults.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct OtelConfigToml {
    /// Log user prompt in traces
    pub log_user_prompt: Option<bool>,

    /// Mark traces with environment (dev, staging, prod, test). Defaults to dev.
    pub environment: Option<String>,

    /// Optional log exporter
    pub exporter: Option<OtelExporterKind>,

    /// Optional trace exporter
    pub trace_exporter: Option<OtelExporterKind>,

    /// Optional metrics exporter
    pub metrics_exporter: Option<OtelExporterKind>,
}

/// Effective OTEL settings after defaults are applied.
#[derive(Debug, Clone, PartialEq)]
pub struct OtelConfig {
    pub log_user_prompt: bool,
    pub environment: String,
    pub exporter: OtelExporterKind,
    pub trace_exporter: OtelExporterKind,
    pub metrics_exporter: OtelExporterKind,
}

impl Default for OtelConfig {
    fn default() -> Self {
        OtelConfig {
            log_user_prompt: false,
            environment: DEFAULT_OTEL_ENVIRONMENT.to_owned(),
            exporter: OtelExporterKind::None,
            trace_exporter: OtelExporterKind::None,
            metrics_exporter: OtelExporterKind::Statsig,
        }
    }
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Notifications {
    Enabled(bool),
    Custom(Vec<String>),
}

impl Default for Notifications {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum NotificationMethod {
    #[default]
    Auto,
    Osc9,
    Bel,
}

impl fmt::Display for NotificationMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotificationMethod::Auto => write!(f, "auto"),
            NotificationMethod::Osc9 => write!(f, "osc9"),
            NotificationMethod::Bel => write!(f, "bel"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum NotificationCondition {
    /// Emit TUI notifications only while the terminal is unfocused.
    #[default]
    Unfocused,
    /// Emit TUI notifications regardless of terminal focus.
    Always,
}

impl fmt::Display for NotificationCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotificationCondition::Unfocused => write!(f, "unfocused"),
            NotificationCondition::Always => write!(f, "always"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct TuiNotificationSettings {
    /// Enable desktop notifications from the TUI.
    /// Defaults to `true`.
    #[serde(default, rename = "notifications")]
    pub notifications: Notifications,

    /// Notification method to use for terminal notifications.
    /// Defaults to `auto`.
    #[serde(default, rename = "notification_method")]
    pub method: NotificationMethod,

    /// Controls whether TUI notifications are delivered only when the terminal is unfocused or
    /// regardless of focus. Defaults to `unfocused`.
    #[serde(default, rename = "notification_condition")]
    pub condition: NotificationCondition,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ModelAvailabilityNuxConfig {
    /// Number of times a startup availability NUX has been shown per model slug.
    #[serde(default, flatten)]
    pub shown_count: HashMap<String, u32>,
}

/// Fallback resize-reflow row cap when Codex cannot identify a terminal-specific scrollback size.
pub const DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS: usize = 1_000;

/// Collection of settings that are specific to the TUI.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Tui {
    #[serde(default, flatten)]
    pub notification_settings: TuiNotificationSettings,

    /// Enable animations (welcome screen, shimmer effects, spinners).
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub animations: bool,

    /// Show startup tooltips in the TUI welcome screen.
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    pub show_tooltips: bool,

    /// Controls whether the TUI uses the terminal's alternate screen buffer.
    ///
    /// - `auto` (default): Disable alternate screen in Zellij, enable elsewhere.
    /// - `always`: Always use alternate screen (original behavior).
    /// - `never`: Never use alternate screen (inline mode only, preserves scrollback).
    ///
    /// Using alternate screen provides a cleaner fullscreen experience but prevents
    /// scrollback in terminal multiplexers like Zellij that follow the xterm spec.
    #[serde(default)]
    pub alternate_screen: AltScreenMode,

    /// Ordered list of status line item identifiers.
    ///
    /// When set, the TUI renders the selected items as the status line.
    /// When unset, the TUI defaults to: `model-with-reasoning` and `current-dir`.
    #[serde(default)]
    pub status_line: Option<Vec<String>>,

    /// Ordered list of terminal title item identifiers.
    ///
    /// When set, the TUI renders the selected items into the terminal window/tab title.
    /// When unset, the TUI defaults to: `activity` and `project`.
    /// The `activity` item spins while working and shows an action-required
    /// message when blocked on the user.
    #[serde(default)]
    pub terminal_title: Option<Vec<String>>,

    /// Syntax highlighting theme name (kebab-case).
    ///
    /// When set, overrides automatic light/dark theme detection.
    /// Use `/theme` in the TUI or see `$CODEX_HOME/themes` for custom themes.
    #[serde(default)]
    pub theme: Option<String>,

    /// Keybinding overrides for the TUI.
    ///
    /// This supports rebinding selected actions globally and by context.
    /// Context bindings take precedence over `global` bindings.
    #[serde(default)]
    pub keymap: TuiKeymap,

    /// Startup tooltip availability NUX state persisted by the TUI.
    #[serde(default)]
    pub model_availability_nux: ModelAvailabilityNuxConfig,

    /// Trim terminal resize-reflow replay to the most recent rendered terminal rows when the
    /// transcript exceeds this cap. Omit to use Codex's terminal-specific default. Set to `0` to
    /// keep all rendered rows.
    #[serde(default)]
    #[schemars(range(min = 0))]
    pub terminal_resize_reflow_max_rows: Option<usize>,
}

const fn default_true() -> bool {
    true
}

/// Settings for notices we display to users via the tui and app-server clients
/// (primarily the Codex IDE extension). NOTE: these are different from
/// notifications - notices are warnings, NUX screens, acknowledgements, etc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ExternalConfigMigrationPrompts {
    /// Tracks whether home-level external config migration prompts are hidden.
    pub home: Option<bool>,
    /// Tracks the last time the home-level external config migration prompt was shown.
    pub home_last_prompted_at: Option<i64>,
    /// Tracks which project paths have opted out of external config migration prompts.
    #[serde(default)]
    pub projects: BTreeMap<String, bool>,
    /// Tracks the last time a project-level external config migration prompt was shown.
    #[serde(default)]
    pub project_last_prompted_at: BTreeMap<String, i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct Notice {
    /// Tracks whether the user has acknowledged the full access warning prompt.
    pub hide_full_access_warning: Option<bool>,
    /// Tracks whether the user has acknowledged the Windows world-writable directories warning.
    pub hide_world_writable_warning: Option<bool>,
    /// Tracks whether the user opted out of Codex-managed fast defaults.
    pub fast_default_opt_out: Option<bool>,
    /// Tracks whether the user opted out of the rate limit model switch reminder.
    pub hide_rate_limit_model_nudge: Option<bool>,
    /// Tracks whether the user has seen the model migration prompt
    pub hide_gpt5_1_migration_prompt: Option<bool>,
    /// Tracks whether the user has seen the gpt-5.1-codex-max migration prompt
    #[serde(rename = "hide_gpt-5.1-codex-max_migration_prompt")]
    pub hide_gpt_5_1_codex_max_migration_prompt: Option<bool>,
    /// Tracks acknowledged model migrations as old->new model slug mappings.
    #[serde(default)]
    pub model_migrations: BTreeMap<String, String>,
    /// Tracks scopes where external config migration prompts should be suppressed.
    #[serde(default)]
    pub external_config_migration_prompts: ExternalConfigMigrationPrompts,
}

pub use crate::enablement_config::EnablementConfig;
pub use crate::enablement_config::EnablementFilterConfig;
pub use crate::enablement_config::EnablementFilterMode;
pub use crate::enablement_config::ModeEnablementConfig;
pub use crate::skills_config::BundledSkillsConfig;
pub use crate::skills_config::SkillConfig;
pub use crate::skills_config::SkillModeFilterConfig;
pub use crate::skills_config::SkillModeFilterMode;
pub use crate::skills_config::SkillsConfig;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PluginConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Per-MCP-server policy overlays for MCP servers contributed by this plugin.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_servers: HashMap<String, PluginMcpServerConfig>,
}

/// Policy settings for a plugin-provided MCP server.
///
/// This intentionally excludes transport settings: plugin manifests own how the
/// MCP server is launched, while user config owns enablement and tool policy.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PluginMcpServerConfig {
    /// When `false`, Codex skips initializing this plugin MCP server.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Approval mode for tools in this server unless a tool override exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools_approval_mode: Option<AppToolApproval>,

    /// Explicit allow-list of tools exposed from this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,

    /// Explicit deny-list of tools. These tools are removed after applying `enabled_tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,

    /// Per-tool approval settings keyed by tool name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tools: HashMap<String, McpServerToolConfig>,
}

impl Default for PluginMcpServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            tools: HashMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct MarketplaceConfig {
    /// Last time Codex successfully added or refreshed this marketplace.
    #[serde(default)]
    pub last_updated: Option<String>,
    /// Git revision Codex last successfully activated for this marketplace.
    #[serde(default)]
    pub last_revision: Option<String>,
    /// Source kind used to install this marketplace.
    #[serde(default)]
    pub source_type: Option<MarketplaceSourceType>,
    /// Source location used when the marketplace was added.
    #[serde(default)]
    pub source: Option<String>,
    /// Git ref to check out when `source_type` is `git`.
    #[serde(default, rename = "ref")]
    pub ref_name: Option<String>,
    /// Sparse checkout paths used when `source_type` is `git`.
    #[serde(default)]
    pub sparse_paths: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MarketplaceSourceType {
    Git,
    Local,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct SandboxWorkspaceWrite {
    #[serde(default)]
    pub writable_roots: Vec<AbsolutePathBuf>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub exclude_tmpdir_env_var: bool,
    #[serde(default)]
    pub exclude_slash_tmp: bool,
}

impl From<SandboxWorkspaceWrite> for codex_app_server_protocol::SandboxSettings {
    fn from(sandbox_workspace_write: SandboxWorkspaceWrite) -> Self {
        Self {
            writable_roots: sandbox_workspace_write.writable_roots,
            network_access: Some(sandbox_workspace_write.network_access),
            exclude_tmpdir_env_var: Some(sandbox_workspace_write.exclude_tmpdir_env_var),
            exclude_slash_tmp: Some(sandbox_workspace_write.exclude_slash_tmp),
        }
    }
}

/// Policy for building the `env` when spawning a process via either the
/// `shell` or `local_shell` tool.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ShellEnvironmentPolicyToml {
    pub inherit: Option<ShellEnvironmentPolicyInherit>,

    pub ignore_default_excludes: Option<bool>,

    /// List of regular expressions.
    pub exclude: Option<Vec<String>>,

    pub r#set: Option<HashMap<String, String>>,

    /// List of regular expressions.
    pub include_only: Option<Vec<String>>,

    pub experimental_use_profile: Option<bool>,
}

impl From<ShellEnvironmentPolicyToml> for ShellEnvironmentPolicy {
    fn from(toml: ShellEnvironmentPolicyToml) -> Self {
        // Default to inheriting the full environment when not specified.
        let inherit = toml.inherit.unwrap_or(ShellEnvironmentPolicyInherit::All);
        let ignore_default_excludes = toml.ignore_default_excludes.unwrap_or(true);
        let exclude = toml
            .exclude
            .unwrap_or_default()
            .into_iter()
            .map(|s| EnvironmentVariablePattern::new_case_insensitive(&s))
            .collect();
        let r#set = toml.r#set.unwrap_or_default();
        let include_only = toml
            .include_only
            .unwrap_or_default()
            .into_iter()
            .map(|s| EnvironmentVariablePattern::new_case_insensitive(&s))
            .collect();
        let use_profile = toml.experimental_use_profile.unwrap_or(false);

        Self {
            inherit,
            ignore_default_excludes,
            exclude,
            r#set,
            include_only,
            use_profile,
        }
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
