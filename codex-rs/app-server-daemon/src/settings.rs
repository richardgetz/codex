use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Map;
use serde_json::Value;
use tokio::fs;

pub(crate) const DEFAULT_UPDATE_INTERVAL_MINUTES: u32 = 60;
pub(crate) const DEFAULT_SHUTDOWN_GRACE_SECONDS: u32 = 60;
pub(crate) const MAX_SHUTDOWN_GRACE_SECONDS: u32 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DaemonSettings {
    pub(crate) remote_control_enabled: bool,
    pub(crate) auto_update_enabled: bool,
    pub(crate) update_interval_minutes: u32,
    pub(crate) shutdown_grace_seconds: u32,
    /// Optional local Codex launcher to use instead of the standalone install.
    ///
    /// A configured launcher opts out of the standalone updater while retaining
    /// the updater settings for a later return to the managed installation.
    pub(crate) managed_codex_path: Option<std::path::PathBuf>,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            remote_control_enabled: false,
            auto_update_enabled: true,
            update_interval_minutes: DEFAULT_UPDATE_INTERVAL_MINUTES,
            shutdown_grace_seconds: DEFAULT_SHUTDOWN_GRACE_SECONDS,
            managed_codex_path: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSettings {
    #[serde(default)]
    remote_control_enabled: bool,
    #[serde(default = "default_shutdown_grace_seconds")]
    shutdown_grace_seconds: u32,
    #[serde(default)]
    updater: UpdaterSettings,
    #[serde(default)]
    managed_codex_path: Option<std::path::PathBuf>,
}

impl Default for StoredSettings {
    fn default() -> Self {
        Self {
            remote_control_enabled: false,
            shutdown_grace_seconds: DEFAULT_SHUTDOWN_GRACE_SECONDS,
            updater: UpdaterSettings::default(),
            managed_codex_path: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdaterSettings {
    #[serde(default = "default_auto_update_enabled")]
    pub(crate) auto_update_enabled: bool,
    #[serde(default = "default_update_interval_minutes")]
    pub(crate) update_interval_minutes: u32,
}

impl Default for UpdaterSettings {
    fn default() -> Self {
        Self {
            auto_update_enabled: true,
            update_interval_minutes: DEFAULT_UPDATE_INTERVAL_MINUTES,
        }
    }
}

fn default_auto_update_enabled() -> bool {
    true
}

fn default_update_interval_minutes() -> u32 {
    DEFAULT_UPDATE_INTERVAL_MINUTES
}

fn default_shutdown_grace_seconds() -> u32 {
    DEFAULT_SHUTDOWN_GRACE_SECONDS
}

fn validate_shutdown_grace(seconds: u32) -> Result<()> {
    ensure!(
        seconds <= MAX_SHUTDOWN_GRACE_SECONDS,
        "shutdown grace must be between 0 and {MAX_SHUTDOWN_GRACE_SECONDS} seconds"
    );
    Ok(())
}

impl UpdaterSettings {
    pub(crate) async fn load(settings_file: &Path) -> Result<Self> {
        let settings: StoredSettings = read_settings(settings_file).await?;
        validate_shutdown_grace(settings.shutdown_grace_seconds)?;
        settings.updater.validate()?;
        Ok(settings.updater)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.update_interval_minutes > 0,
            "update interval must be positive"
        );
        Ok(())
    }

    pub(crate) fn update_interval(&self, minute: Duration) -> Duration {
        minute * self.update_interval_minutes
    }
}

impl DaemonSettings {
    pub(crate) async fn load(path: &Path) -> Result<Self> {
        let stored: StoredSettings = read_settings(path).await?;
        stored.updater.validate()?;
        validate_shutdown_grace(stored.shutdown_grace_seconds)?;
        let settings = Self {
            remote_control_enabled: stored.remote_control_enabled,
            auto_update_enabled: stored.updater.auto_update_enabled,
            update_interval_minutes: stored.updater.update_interval_minutes,
            shutdown_grace_seconds: stored.shutdown_grace_seconds,
            managed_codex_path: stored.managed_codex_path,
        };
        settings.validate_launcher()?;
        Ok(settings)
    }

    /// Load only the stop grace setting. Stop must remain available while a
    /// settings file is incomplete or contains a newer incompatible field.
    pub(crate) async fn load_for_stop(path: &Path) -> Self {
        #[derive(Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StopSettings {
            shutdown_grace_seconds: Option<u32>,
        }

        let shutdown_grace_seconds = read_settings::<StopSettings>(path)
            .await
            .ok()
            .and_then(|settings| settings.shutdown_grace_seconds)
            .filter(|seconds| *seconds <= MAX_SHUTDOWN_GRACE_SECONDS)
            .unwrap_or(DEFAULT_SHUTDOWN_GRACE_SECONDS);
        Self {
            shutdown_grace_seconds,
            ..Self::default()
        }
    }

    pub(crate) async fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "failed to create daemon settings directory {}",
                    parent.display()
                )
            })?;
        }
        let mut settings: Map<String, Value> = read_settings(path).await?;
        settings.insert(
            "remoteControlEnabled".to_string(),
            Value::Bool(self.remote_control_enabled),
        );
        settings.insert(
            "shutdownGraceSeconds".to_string(),
            Value::from(self.shutdown_grace_seconds),
        );
        settings.insert(
            "updater".to_string(),
            serde_json::json!({
                "autoUpdateEnabled": self.auto_update_enabled,
                "updateIntervalMinutes": self.update_interval_minutes,
            }),
        );
        match &self.managed_codex_path {
            Some(path) => {
                settings.insert(
                    "managedCodexPath".to_string(),
                    Value::String(path.to_string_lossy().into_owned()),
                );
            }
            None => {
                settings.remove("managedCodexPath");
            }
        }
        let temporary_path = path.with_extension("tmp");
        let contents =
            serde_json::to_vec_pretty(&settings).context("failed to serialize settings")?;
        fs::write(&temporary_path, contents)
            .await
            .with_context(|| {
                format!(
                    "failed to write daemon settings {}",
                    temporary_path.display()
                )
            })?;
        fs::rename(&temporary_path, path)
            .await
            .with_context(|| format!("failed to replace daemon settings {}", path.display()))
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_launcher()?;
        self.updater().validate()?;
        validate_shutdown_grace(self.shutdown_grace_seconds)
    }

    fn validate_launcher(&self) -> Result<()> {
        if let Some(path) = &self.managed_codex_path {
            ensure!(
                path.is_absolute(),
                "configured Codex launcher must be an absolute path: {}",
                path.display()
            );
        }
        Ok(())
    }

    pub(crate) fn updater(&self) -> UpdaterSettings {
        UpdaterSettings {
            auto_update_enabled: self.auto_update_enabled,
            update_interval_minutes: self.update_interval_minutes,
        }
    }
}

async fn read_settings<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let contents = match fs::read_to_string(path).await {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read settings {}", path.display()));
        }
    };
    serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse settings {}", path.display()))
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
