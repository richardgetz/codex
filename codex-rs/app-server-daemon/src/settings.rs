use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use tokio::fs;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonSettings {
    pub(crate) remote_control_enabled: bool,
    /// Optional local Codex launcher to use instead of the standalone install.
    ///
    /// This path is configured by a local CLI invocation and is intentionally
    /// persisted as supplied. In particular, a package-manager shim or symlink
    /// must be re-resolved for each daemon start so an external upgrade can
    /// replace the target without changing daemon settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) managed_codex_path: Option<std::path::PathBuf>,
}

impl DaemonSettings {
    pub(crate) async fn load(path: &Path) -> Result<Self> {
        let contents = match fs::read_to_string(path).await {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read daemon settings {}", path.display()));
            }
        };

        let settings = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse daemon settings {}", path.display()))?;
        settings.validate()?;
        Ok(settings)
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

        let contents = serde_json::to_vec_pretty(self).context("failed to serialize settings")?;
        fs::write(path, contents)
            .await
            .with_context(|| format!("failed to write daemon settings {}", path.display()))
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(path) = &self.managed_codex_path {
            anyhow::ensure!(
                path.is_absolute(),
                "configured Codex launcher must be an absolute path: {}",
                path.display()
            );
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use pretty_assertions::assert_eq;

    use super::DaemonSettings;

    #[test]
    fn daemon_settings_use_camel_case_json() {
        assert_eq!(
            serde_json::to_string(&DaemonSettings {
                remote_control_enabled: true,
                managed_codex_path: None,
            })
            .expect("serialize"),
            r#"{"remoteControlEnabled":true}"#
        );
    }

    #[tokio::test]
    async fn legacy_settings_load_without_launcher() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("settings.json");
        tokio::fs::write(&path, br#"{"remoteControlEnabled":true}"#)
            .await
            .expect("write settings");

        assert_eq!(
            DaemonSettings::load(&path).await.expect("load settings"),
            DaemonSettings {
                remote_control_enabled: true,
                managed_codex_path: None,
            }
        );
    }

    #[test]
    fn configured_launcher_uses_camel_case_json() {
        assert_eq!(
            serde_json::to_string(&DaemonSettings {
                remote_control_enabled: true,
                managed_codex_path: Some("/opt/homebrew/bin/codex-rick".into()),
            })
            .expect("serialize"),
            r#"{"remoteControlEnabled":true,"managedCodexPath":"/opt/homebrew/bin/codex-rick"}"#
        );
    }

    #[tokio::test]
    async fn configured_launcher_round_trips_settings() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("settings.json");
        let expected = DaemonSettings {
            remote_control_enabled: true,
            managed_codex_path: Some("/opt/homebrew/bin/codex-rick".into()),
        };

        expected.save(&path).await.expect("save settings");

        assert_eq!(
            DaemonSettings::load(&path).await.expect("load settings"),
            expected
        );
    }

    #[test]
    fn configured_launcher_must_be_absolute() {
        let error = DaemonSettings {
            remote_control_enabled: false,
            managed_codex_path: Some("codex-rick".into()),
        }
        .validate()
        .expect_err("relative launcher should be rejected");
        assert!(error.to_string().contains("absolute path"));
    }
}
