use super::ResumePermissions;
use crate::legacy_core::config::Config;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::ConfigOverrides;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::path::Path;

async fn build_user_profile_config(
    codex_home: &Path,
    profile_config_path: &Path,
    profile: &str,
) -> color_eyre::Result<Config> {
    let config = ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .loader_overrides(codex_config::LoaderOverrides {
            user_config_path: Some(AbsolutePathBuf::try_from(profile_config_path.to_path_buf())?),
            user_config_profile: Some(profile.parse()?),
            ..codex_config::LoaderOverrides::without_managed_config_for_tests()
        })
        .build()
        .await?;
    Ok(config)
}

#[tokio::test]
async fn selected_profile_permissions_override_saved_permissions() -> color_eyre::Result<()> {
    let home = tempfile::tempdir()?;
    let profile = home.path().join("work.config.toml");
    std::fs::write(
        &profile,
        "default_permissions = \":read-only\"\napproval_policy = \"never\"\napprovals_reviewer = \"auto_review\"\n",
    )?;
    let config = build_user_profile_config(home.path(), &profile, "work").await?;
    assert_eq!(
        ResumePermissions::from_overrides(&config, &ConfigOverrides::default()),
        ResumePermissions {
            approval_policy: true,
            approvals_reviewer: true,
            profile: true,
            ..Default::default()
        },
    );

    let ordinary_user_builder = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .loader_overrides(codex_config::LoaderOverrides {
            user_config_path: Some(AbsolutePathBuf::try_from(profile.clone())?),
            ..codex_config::LoaderOverrides::without_managed_config_for_tests()
        });
    let ordinary_user_config = ordinary_user_builder.clone().build().await?;
    assert_eq!(
        ResumePermissions::from_overrides(
            &ordinary_user_config,
            &ConfigOverrides::default()
        ),
        ResumePermissions::default(),
        "ordinary user defaults should not replace saved thread permissions"
    );

    let sandbox_profile = home.path().join("sandbox.config.toml");
    std::fs::write(&sandbox_profile, "sandbox_mode = 'workspace-write'\n")?;
    let sandbox_config =
        build_user_profile_config(home.path(), &sandbox_profile, "sandbox").await?;
    assert_eq!(
        ResumePermissions::from_overrides(&sandbox_config, &ConfigOverrides::default()),
        ResumePermissions {
            profile: true,
            ..Default::default()
        },
        "selected sandbox profile should override saved thread permissions"
    );

    let permission_home = tempfile::tempdir()?;
    std::fs::write(
        permission_home.path().join("config.toml"),
        "default_permissions = 'safe'\n\n[permissions.safe]\n",
    )?;
    for (profile_name, profile_contents, expected_permissions) in [
        (
            "active-permission",
            "[permissions.safe]\nextends = ':read-only'\n",
            ResumePermissions {
                profile: true,
                ..Default::default()
            },
        ),
        (
            "dormant-permission",
            "[permissions.unused]\nextends = ':read-only'\n",
            ResumePermissions::default(),
        ),
        (
            "description-only",
            "[permissions.safe]\ndescription = 'metadata only'\n",
            ResumePermissions::default(),
        ),
    ] {
        let profile_path = permission_home
            .path()
            .join(format!("{profile_name}.config.toml"));
        std::fs::write(&profile_path, profile_contents)?;
        let config =
            build_user_profile_config(permission_home.path(), &profile_path, profile_name).await?;
        assert_eq!(
            config
                .permissions
                .active_permission_profile()
                .map(|profile| profile.id),
            Some("safe".to_string())
        );
        assert_eq!(
            ResumePermissions::from_overrides(&config, &ConfigOverrides::default()),
            expected_permissions,
            "selected profile {profile_name} permission origin should be classified correctly"
        );
    }

    for (permission_id, profile_name, profile_contents) in [
        (
            "safe",
            "roots",
            "[permissions.safe.workspace_roots]\n",
        ),
        (
            "safe.work",
            "dotted-roots",
            "[permissions.\"safe.work\".workspace_roots]\n",
        ),
    ] {
        std::fs::write(
            permission_home.path().join("config.toml"),
            format!("default_permissions = '{permission_id}'\n"),
        )?;
        let profile_path = permission_home
            .path()
            .join(format!("{profile_name}.config.toml"));
        std::fs::write(&profile_path, profile_contents)?;
        let config =
            build_user_profile_config(permission_home.path(), &profile_path, profile_name).await?;
        assert_eq!(
            config
                .permissions
                .active_permission_profile()
                .map(|profile| profile.id),
            Some(permission_id.to_string())
        );
        assert_eq!(
            ResumePermissions::from_overrides(&config, &ConfigOverrides::default()),
            ResumePermissions {
                profile: true,
                ..Default::default()
            },
            "empty roots for active permission profile {permission_id} should only override profile settings"
        );
    }
    let direct_rules = ordinary_user_builder
        .clone()
        .cli_overrides(vec![(
            "permissions.safe.extends".into(),
            ":read-only".into(),
        )])
        .build()
        .await?;
    assert_eq!(
        ResumePermissions::from_overrides(&direct_rules, &ConfigOverrides::default()),
        ResumePermissions {
            profile: true,
            ..Default::default()
        },
    );
    let config = ordinary_user_builder
        .cli_overrides(vec![("approval_policy".into(), "on-request".into())])
        .build()
        .await?;
    assert_eq!(
        ResumePermissions::from_overrides(
            &config,
            &ConfigOverrides {
                default_permissions: Some(":read-only".into()),
                cwd: Some(home.path().to_path_buf()),
                ..Default::default()
            },
        ),
        ResumePermissions {
            approval_policy: true,
            profile: true,
            workspace_roots: true,
            ..Default::default()
        },
    );
    Ok(())
}
