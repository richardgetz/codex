//! Tracks explicit permission choices from selected profiles and direct launch input separately
//! from ordinary user config defaults. Omitted choices let app-server restore the destination
//! task's saved settings.

use crate::legacy_core::config::Config;
use crate::legacy_core::config::ConfigOverrides;
use codex_config::ConfigLayerSource;

fn config_value_at_path<'a>(config: &'a toml::Value, path: &[&str]) -> Option<&'a toml::Value> {
    path.iter().try_fold(config, |value, key| value.get(*key))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResumePermissions {
    pub approval_policy: bool,
    pub approvals_reviewer: bool,
    pub profile: bool,
    pub workspace_roots: bool,
}

impl ResumePermissions {
    /// Resolve selected-profile, SessionFlags, and direct launch permission choices.
    pub(crate) fn from_overrides(config: &Config, overrides: &ConfigOverrides) -> Self {
        let effective_config = config.config_layer_stack.effective_config();
        let active_permission_profile_id = config
            .permissions
            .active_permission_profile()
            .map(|profile| profile.id);
        let empty_container_from_source =
            |path: &[&str], is_override: &dyn Fn(&ConfigLayerSource) -> bool| {
                let effective_is_empty = config_value_at_path(&effective_config, path).is_some_and(
                    |value| match value {
                        toml::Value::Array(items) => items.is_empty(),
                        toml::Value::Table(table) => table.is_empty(),
                        _ => false,
                    },
                );
                let highest_layer_is_override = config
                    .config_layer_stack
                    .layers_high_to_low()
                    .find_map(|layer| {
                        config_value_at_path(&layer.config, path).map(|_| is_override(&layer.name))
                    })
                    .unwrap_or(false);
                effective_is_empty && highest_layer_is_override
            };
        let active_permission_profile_settings_override = active_permission_profile_id
            .as_deref()
            .is_some_and(|profile_id| {
                // Only the active named profile's settings are forwarded; dormant definitions
                // in the selected user profile must not replace the saved thread profile.
                let path = ["permissions", profile_id];
                let origins = config
                    .config_layer_stack
                    .origins_with_path_filter(|origin_path| {
                        origin_path.len() > path.len()
                            && path
                                .iter()
                                .zip(origin_path)
                                .all(|(path_segment, origin_segment)| {
                                    origin_segment.as_str() == *path_segment
                                })
                            && matches!(
                                origin_path[path.len()].as_str(),
                                "extends" | "workspace_roots" | "filesystem" | "network"
                            )
                    });
                origins.values().any(|origin| {
                    matches!(
                        &origin.name,
                        ConfigLayerSource::SessionFlags
                            | ConfigLayerSource::User {
                                profile: Some(_),
                                ..
                            }
                    )
                }) || empty_container_from_source(
                    &["permissions", profile_id, "workspace_roots"],
                    &|source| {
                        matches!(
                            source,
                            ConfigLayerSource::SessionFlags
                                | ConfigLayerSource::User {
                                    profile: Some(_),
                                    ..
                                }
                        )
                    },
                )
            });
        // Empty arrays/tables have no leaf origin, so confirm the winning layer directly.
        let has = |path: &str| {
            let path_segments = path.split('.').collect::<Vec<_>>();
            let origins = config
                .config_layer_stack
                .origins_with_path_filter(|origin_path| {
                    origin_path.len() >= path_segments.len()
                        && path_segments.iter().zip(origin_path).all(
                            |(path_segment, origin_segment)| {
                                origin_segment.as_str() == *path_segment
                            },
                        )
                });
            let from_session_flags = origins
                .values()
                .any(|origin| matches!(&origin.name, ConfigLayerSource::SessionFlags))
                || empty_container_from_source(&path_segments, &|source| {
                    matches!(source, ConfigLayerSource::SessionFlags)
                });
            // Only fields supported by user profiles should replace saved thread permissions.
            // Filtered origins record winning leaves, so supported profile tables match descendants.
            // `network` remains SessionFlags-only because it is not a user profile setting.
            let supported_profile_path = matches!(
                path,
                "approval_policy"
                    | "approvals_reviewer"
                    | "default_permissions"
                    | "sandbox_mode"
                    | "sandbox_workspace_write"
                    | "sandbox_workspace_write.writable_roots"
            );
            let from_user_profile = supported_profile_path
                && (origins.values().any(|origin| {
                    matches!(
                        &origin.name,
                        ConfigLayerSource::User {
                            profile: Some(_),
                            ..
                        }
                    )
                }) || empty_container_from_source(&path_segments, &|source| {
                    matches!(
                        source,
                        ConfigLayerSource::User {
                            profile: Some(_),
                            ..
                        }
                    )
                }))
                || (path == "sandbox_workspace_write"
                    && empty_container_from_source(
                        &["sandbox_workspace_write", "writable_roots"],
                        &|source| {
                            matches!(
                                source,
                                ConfigLayerSource::User {
                                    profile: Some(_),
                                    ..
                                }
                            )
                        },
                    ))
                || (path == "permissions" && active_permission_profile_settings_override);
            from_session_flags || from_user_profile
        };
        Self {
            approval_policy: overrides.approval_policy.is_some() || has("approval_policy"),
            approvals_reviewer: overrides.approvals_reviewer.is_some() || has("approvals_reviewer"),
            profile: overrides.sandbox_mode.is_some()
                || overrides.permission_profile.is_some()
                || overrides.default_permissions.is_some()
                || !overrides.additional_writable_roots.is_empty()
                || has("default_permissions")
                || has("sandbox_mode")
                || has("sandbox_workspace_write")
                || has("permissions")
                || has("network"),
            workspace_roots: overrides.cwd.is_some()
                || overrides.workspace_roots.is_some()
                || !overrides.additional_writable_roots.is_empty()
                || has("sandbox_workspace_write.writable_roots"),
        }
    }

    #[cfg(test)]
    pub(crate) const CURRENT_CONFIG: Self = Self {
        approval_policy: true,
        approvals_reviewer: true,
        profile: true,
        workspace_roots: true,
    };
}

#[cfg(test)]
#[path = "resume_permissions_tests.rs"]
mod tests;
