use super::*;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::collections::BTreeMap;

#[test]
fn daemon_recovery_requires_explicit_restart_and_defaults_to_cancel() {
    let issue = CompatibilityError {
        reason: "This session requires api_key_model_discovery to be enabled".to_string(),
        restart_features: Some(BTreeMap::from([
            ("api_key_model_discovery".to_string(), true),
            ("mcp_oauth_refresh_coordination".to_string(), false),
        ])),
    };
    let non_restartable = CompatibilityError {
        reason: "code-mode host fallback policy requires embedded mode".to_string(),
        restart_features: None,
    };
    let launcher_update = CompatibilityError {
        reason: "safe daemon update returned NeedsAttention; selected launcher /opt/homebrew/bin/codex-rick (installed version 0.157.1-rick.2) is running as version 0.156.1-rick.2 (app-server version 0.156.1). The matching handoff journal is missing; Codex preserved the receipt and did not replay or discard its saved sessions. thread/handoff/status failed: unknown handoff id handoff-1".to_string(),
        restart_features: None,
    };
    for (issue, managed, snapshot) in [
        (&issue, true, "daemon_recovery_menu"),
        (&issue, false, "unmanaged_daemon_recovery_menu"),
        (
            &non_restartable,
            true,
            "non_restartable_daemon_recovery_menu",
        ),
        (
            &launcher_update,
            true,
            "launcher_update_failure_recovery_menu",
        ),
    ] {
        let mut view = recovery_view(issue, managed, &RuntimeKeymap::defaults());
        let area = Rect::new(
            /*x*/ 0,
            /*y*/ 0,
            /*width*/ 90,
            view.desired_height(/*width*/ 90),
        );
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = buffer
            .content
            .chunks(90)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(snapshot, text);
        view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(view.take_last_selected_index(), Some(2));

        let mut view = recovery_view(issue, managed, &RuntimeKeymap::defaults());
        view.handle_key_event(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        if managed && issue.restart_features.is_some() {
            assert!(!view.is_complete());
            view.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(view.take_last_selected_index(), Some(1));
        } else {
            assert_eq!(view.take_last_selected_index(), Some(2));
        }
    }
}
