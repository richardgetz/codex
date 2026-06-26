use super::*;
use crate::context::ContextualUserFragment;
use crate::context::world_state::WorldState;
use anyhow::Result;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::TurnContextItem;
use codex_utils_absolute_path::test_support::PathBufExt;
use core_test_support::test_path_buf;
use pretty_assertions::assert_eq;

#[test]
fn renders_full_environment_state() -> Result<()> {
    let context = EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "zsh")?),
            (
                "devbox".to_string(),
                available("file:///workspace", "bash")?,
            ),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let mut world_state = WorldState::default();
    world_state.add_section(context);

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <environments>
    <environment id="devbox">
      <cwd>/workspace</cwd>
      <shell>bash</shell>
    </environment>
    <environment id="laptop">
      <cwd>/repo</cwd>
      <shell>zsh</shell>
    </environment>
  </environments>
</environment_context>"#,
        )],
        render_fragments(world_state.render_full()),
    );
    Ok(())
}

#[test]
fn renders_only_changed_environments() -> Result<()> {
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "bash")?),
            ("devbox".to_string(), starting("file:///workspace")?),
            ("old".to_string(), available("file:///old", "sh")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    });
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState {
        environments: [
            ("laptop".to_string(), available("file:///repo", "zsh")?),
            (
                "devbox".to_string(),
                available("file:///workspace", "powershell")?,
            ),
            ("remote".to_string(), starting("file:///remote")?),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    });

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <environments>
    <environment id="devbox">
      <cwd>/workspace</cwd>
      <shell>powershell</shell>
    </environment>
    <environment id="laptop">
      <cwd>/repo</cwd>
      <shell>zsh</shell>
    </environment>
    <environment id="old" status="unavailable" />
    <environment id="remote">
      <cwd>/remote</cwd>
      <status>starting</status>
    </environment>
  </environments>
</environment_context>"#,
        )],
        render_fragments(current.render_diff(&previous)),
    );
    Ok(())
}

#[test]
fn persisted_turn_context_values_render_a_diff() -> Result<()> {
    let environments = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut previous = WorldState::default();
    previous.add_section(EnvironmentsState {
        current_date: Some("2026-06-19".to_string()),
        timezone: Some("UTC".to_string()),
        network: Some(NetworkContext::new(
            vec!["old.example.com".to_string()],
            vec![],
        )),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::Disabled,
            &[],
        )),
        ..environments.clone()
    });
    let mut current = WorldState::default();
    current.add_section(EnvironmentsState {
        current_date: Some("2026-06-20".to_string()),
        timezone: Some("America/Los_Angeles".to_string()),
        network: Some(NetworkContext::new(
            vec!["new.example.com".to_string()],
            vec!["blocked.example.com".to_string()],
        )),
        filesystem: Some(FileSystemContext::from_permission_profile(
            &PermissionProfile::External {
                network: NetworkSandboxPolicy::Restricted,
            },
            &[],
        )),
        ..environments
    });

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <current_date>2026-06-20</current_date>
  <timezone>America/Los_Angeles</timezone>
  <network enabled="true"><allowed>new.example.com</allowed><denied>blocked.example.com</denied></network>
  <filesystem><permission_profile type="external"><file_system type="external" /></permission_profile></filesystem>
</environment_context>"#,
        )],
        render_fragments(current.render_diff(&previous)),
    );
    Ok(())
}

#[test]
fn subagent_only_change_renders_a_diff() -> Result<()> {
    let environments = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut previous = WorldState::default();
    previous.add_section(environments.clone().with_subagents("- old".to_string()));
    let mut current = WorldState::default();
    current.add_section(environments.with_subagents("- new".to_string()));

    assert_eq!(
        vec![user_message(
            r#"<environment_context>
  <subagents>
    - new
  </subagents>
</environment_context>"#,
        )],
        render_fragments(current.render_diff(&previous)),
    );
    Ok(())
}

#[test]
fn turn_context_item_does_not_seed_unknown_environment_id() {
    let cwd = test_path_buf("/remote/workspace").abs();
    let item = TurnContextItem {
        turn_id: None,
        trace_id: None,
        cwd: cwd.clone(),
        workspace_roots: None,
        current_date: Some("2026-06-26".to_string()),
        timezone: Some("America/New_York".to_string()),
        approval_policy: codex_protocol::protocol::AskForApproval::Never,
        sandbox_policy: codex_protocol::protocol::SandboxPolicy::new_read_only_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: "gpt-5".to_string(),
        comp_hash: None,
        personality: None,
        collaboration_mode: None,
        multi_agent_version: None,
        multi_agent_mode: None,
        realtime_active: None,
        effort: None,
        summary: codex_protocol::config_types::ReasoningSummary::Auto,
        user_instructions: None,
        developer_instructions: None,
        final_output_json_schema: None,
        truncation_policy: None,
    };

    let expected = format!(
        r#"<environment_context>
  <current_date>2026-06-26</current_date>
  <timezone>America/New_York</timezone>
  <filesystem><workspace_roots><root>{}</root></workspace_roots><permission_profile type="managed"><file_system type="restricted"><entry access="read"><special>:root</special></entry></file_system></permission_profile></filesystem>
</environment_context>"#,
        cwd.to_string_lossy()
    );
    let rendered = ContextualUserFragment::into_boxed_response_item(Box::new(
        EnvironmentsState::from_turn_context_item(&item),
    )
        as Box<dyn ContextualUserFragment>);

    assert_eq!(user_message(&expected), rendered,);
}

#[test]
fn single_environment_diff_ignores_unknown_shell() -> Result<()> {
    let previous = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            EnvironmentState {
                cwd: PathUri::parse("file:///repo")?,
                status: EnvironmentStatus::Available,
                shell: None,
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let current = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "zsh")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    assert_eq!(
        None,
        render_fragment(WorldStateSection::render_diff(&current, Some(&previous)))
    );
    Ok(())
}

#[test]
fn removed_legacy_environment_renders_unavailable() -> Result<()> {
    let previous = EnvironmentsState {
        environments: [(
            LOCAL_ENVIRONMENT_ID.to_string(),
            available("file:///repo", "bash")?,
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    assert_eq!(
        Some(user_message(
            r#"<environment_context>
  <environments>
    <environment id="local" status="unavailable" />
  </environments>
</environment_context>"#,
        )),
        render_fragment(WorldStateSection::render_diff(
            &EnvironmentsState::default(),
            Some(&previous),
        )),
    );
    Ok(())
}

fn available(cwd: &str, shell: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Available,
        shell: Some(shell.to_string()),
    })
}

fn starting(cwd: &str) -> Result<EnvironmentState> {
    Ok(EnvironmentState {
        cwd: PathUri::parse(cwd)?,
        status: EnvironmentStatus::Starting,
        shell: None,
    })
}

fn render_fragments(fragments: Vec<Box<dyn ContextualUserFragment>>) -> Vec<ResponseItem> {
    fragments
        .into_iter()
        .map(ContextualUserFragment::into_boxed_response_item)
        .collect()
}

fn render_fragment(fragment: Option<Box<dyn ContextualUserFragment>>) -> Option<ResponseItem> {
    fragment.map(ContextualUserFragment::into_boxed_response_item)
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}
