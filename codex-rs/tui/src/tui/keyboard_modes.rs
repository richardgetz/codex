//! Terminal keyboard enhancement setup and teardown helpers.
//!
//! The TUI uses crossterm's keyboard enhancement stack while it owns the terminal, but
//! process exit gets a stronger reset so the parent shell does not inherit enhanced key
//! reporting if a terminal misses the normal stack pop.

use std::fmt;
use std::io::stdout;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_terminal_detection::TerminalName;
use codex_terminal_detection::terminal_info;
use crossterm::Command;
use crossterm::event::KeyboardEnhancementFlags;
use crossterm::event::PopKeyboardEnhancementFlags;
use crossterm::event::PushKeyboardEnhancementFlags;
use ratatui::crossterm::execute;

const DISABLE_KEYBOARD_ENHANCEMENT_ENV_VAR: &str = "CODEX_TUI_DISABLE_KEYBOARD_ENHANCEMENT";
const CMUX_WORKSPACE_ID_ENV_VAR: &str = "CMUX_WORKSPACE_ID";
const CMUX_SURFACE_ID_ENV_VAR: &str = "CMUX_SURFACE_ID";
static REALTIME_VOICE_ENABLED: AtomicBool = AtomicBool::new(false);
static RIGHT_OPTION_MONITOR_ENABLED: AtomicBool = AtomicBool::new(false);

pub(super) fn set_realtime_voice_enabled(enabled: bool) {
    REALTIME_VOICE_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(super) fn set_right_option_monitor_enabled(enabled: bool) {
    RIGHT_OPTION_MONITOR_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(super) fn reconfigure_keyboard_enhancement() {
    restore_keyboard_enhancement_stack();
    enable_keyboard_enhancement();
}

pub(super) fn keyboard_enhancement_disabled() -> bool {
    let disable_env = std::env::var(DISABLE_KEYBOARD_ENHANCEMENT_ENV_VAR).ok();
    let is_wsl = running_in_wsl();
    let is_vscode_terminal = is_wsl && running_in_vscode_terminal();
    keyboard_enhancement_disabled_for(disable_env.as_deref(), is_wsl, is_vscode_terminal)
}

fn keyboard_enhancement_disabled_for(
    disable_env: Option<&str>,
    is_wsl: bool,
    is_vscode_terminal: bool,
) -> bool {
    if let Some(disabled) = parse_bool_env(disable_env) {
        return disabled;
    }

    // VS Code running a WSL shell can hide TERM_PROGRAM from the Linux process
    // environment, so `running_in_vscode_terminal` also probes the Windows-side
    // environment through WSL interop.
    is_wsl && is_vscode_terminal
}

fn parse_bool_env(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim) {
        Some("1") => Some(true),
        Some(value) if value.eq_ignore_ascii_case("true") => Some(true),
        Some(value) if value.eq_ignore_ascii_case("yes") => Some(true),
        Some("0") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("false") => Some(false),
        Some(value) if value.eq_ignore_ascii_case("no") => Some(false),
        _ => None,
    }
}

fn running_in_wsl() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::clipboard_paste::is_probably_wsl()
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub(super) fn running_in_vscode_terminal() -> bool {
    vscode_terminal_detected(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        windows_term_program().as_deref(),
    )
}

fn vscode_terminal_detected(
    linux_term_program: Option<&str>,
    windows_term_program: Option<&str>,
) -> bool {
    term_program_is_vscode(linux_term_program) || term_program_is_vscode(windows_term_program)
}

fn term_program_is_vscode(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("vscode"))
}

fn windows_term_program() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        static WINDOWS_TERM_PROGRAM: std::sync::OnceLock<Option<String>> =
            std::sync::OnceLock::new();
        WINDOWS_TERM_PROGRAM
            .get_or_init(read_windows_term_program)
            .clone()
    }

    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn read_windows_term_program() -> Option<String> {
    let output = std::process::Command::new("cmd.exe")
        .args(["/d", "/s", "/c", "set TERM_PROGRAM"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.trim_end_matches('\r')
                .strip_prefix("TERM_PROGRAM=")
                .map(str::to_string)
        })
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn enable_keyboard_enhancement() {
    if keyboard_enhancement_disabled() {
        return;
    }

    let running_in_tmux_session = running_in_tmux_session();
    let tmux_extended_keys_format = if running_in_tmux_session {
        read_tmux_extended_keys_format()
    } else {
        None
    };
    let realtime_voice_enabled = REALTIME_VOICE_ENABLED.load(Ordering::Relaxed);
    let terminal = terminal_info();
    let kitty_terminal_protocol_supported = matches!(terminal.name, TerminalName::Kitty)
        && (std::env::var_os("KITTY_WINDOW_ID").is_some()
            || terminal
                .term_program
                .as_deref()
                .is_some_and(|term_program| term_program.eq_ignore_ascii_case("kitty")));
    let all_keys_supported = should_enable_all_keys(
        terminal.name,
        kitty_terminal_protocol_supported,
        running_in_tmux_session,
        running_in_cmux_session(),
        RIGHT_OPTION_MONITOR_ENABLED.load(Ordering::Relaxed),
    );

    let _ = execute!(
        stdout(),
        DisableModifyOtherKeys,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
            terminal.name,
            all_keys_supported,
            running_in_tmux_session,
            tmux_extended_keys_format.as_deref(),
            realtime_voice_enabled,
        ))
    );

    if tmux_should_enable_modify_other_keys_for(
        running_in_tmux_session,
        tmux_extended_keys_format.as_deref(),
    ) {
        let _ = execute!(stdout(), EnableModifyOtherKeys);
    }
}

fn keyboard_enhancement_flags(
    terminal_name: TerminalName,
    all_keys_supported: bool,
    running_in_tmux_session: bool,
    tmux_extended_keys_format: Option<&str>,
    realtime_voice_enabled: bool,
) -> KeyboardEnhancementFlags {
    let mut flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;

    // Physical modifier-only keys, including macOS Right Option, are only reported
    // when all key events are encoded as CSI-u. Direct terminals use that mode for
    // live voice when no macOS key-state monitor is available; the monitor provides
    // Right Option separately so remote bridges can stay on the safer fallback.
    if realtime_voice_enabled && all_keys_supported {
        flags |= KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        flags | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
    } else if running_in_tmux_session && matches!(tmux_extended_keys_format, Some("xterm")) {
        if realtime_voice_enabled {
            flags | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        } else {
            flags
        }
    } else if matches!(terminal_name, TerminalName::Ghostty | TerminalName::Iterm2) {
        if realtime_voice_enabled {
            flags | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        } else {
            flags
        }
    } else {
        // Preserve repeat classification on transports that support it.
        flags | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
    }
}

fn direct_terminal_all_keys_supported(
    terminal_name: TerminalName,
    kitty_terminal_protocol_supported: bool,
    running_in_tmux_session: bool,
    running_in_cmux_session: bool,
) -> bool {
    if running_in_cmux_session || running_in_tmux_session {
        return false;
    }

    kitty_terminal_protocol_supported
        || matches!(terminal_name, TerminalName::Ghostty | TerminalName::Iterm2)
}

fn should_enable_all_keys(
    terminal_name: TerminalName,
    kitty_terminal_protocol_supported: bool,
    running_in_tmux_session: bool,
    running_in_cmux_session: bool,
    right_option_monitor_enabled: bool,
) -> bool {
    direct_terminal_all_keys_supported(
        terminal_name,
        kitty_terminal_protocol_supported,
        running_in_tmux_session,
        running_in_cmux_session,
    ) && !right_option_monitor_enabled
}

fn running_in_cmux_session() -> bool {
    cmux_session_detected(
        std::env::var(CMUX_WORKSPACE_ID_ENV_VAR).ok().as_deref(),
        std::env::var(CMUX_SURFACE_ID_ENV_VAR).ok().as_deref(),
    )
}

fn cmux_session_detected(workspace_id: Option<&str>, surface_id: Option<&str>) -> bool {
    workspace_id.is_some_and(|value| !value.trim().is_empty())
        || surface_id.is_some_and(|value| !value.trim().is_empty())
}

fn running_in_tmux_session() -> bool {
    tmux_session_detected(
        std::env::var("TMUX").ok().as_deref(),
        std::env::var("TMUX_PANE").ok().as_deref(),
    )
}

fn tmux_session_detected(tmux: Option<&str>, tmux_pane: Option<&str>) -> bool {
    tmux.is_some() || tmux_pane.is_some()
}

fn tmux_should_enable_modify_other_keys_for(
    running_in_tmux_session: bool,
    extended_keys_format: Option<&str>,
) -> bool {
    // Only request mode 2 when tmux confirms csi-u formatting. Older tmux
    // versions do not expose this option and may emit xterm-style sequences,
    // which crossterm does not parse consistently for modified keys.
    running_in_tmux_session && matches!(extended_keys_format, Some("csi-u"))
}

fn read_tmux_extended_keys_format() -> Option<String> {
    if !read_tmux_extended_keys_enabled()? {
        return None;
    }

    for args in [
        ["display-message", "-p", "#{extended-keys-format}"],
        ["show-options", "-gqv", "extended-keys-format"],
    ] {
        let output = std::process::Command::new("tmux")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;

        if !output.status.success() {
            continue;
        }

        if let Some(value) = String::from_utf8(output.stdout)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Some(value);
        }
    }

    None
}

fn read_tmux_extended_keys_enabled() -> Option<bool> {
    for args in [
        ["display-message", "-p", "#{extended-keys}"],
        ["show-options", "-gqv", "extended-keys"],
    ] {
        let output = match std::process::Command::new("tmux")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Ok(output) => output,
            Err(_) => continue,
        };

        if !output.status.success() {
            continue;
        }

        let value = match String::from_utf8(output.stdout) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if let Some(enabled) = parse_tmux_bool(value.trim()) {
            return Some(enabled);
        }
    }

    None
}

fn parse_tmux_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("on")
        || value.eq_ignore_ascii_case("always")
        || value.eq_ignore_ascii_case("true")
        || value == "1"
    {
        Some(true)
    } else if value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("false")
        || value == "0"
    {
        Some(false)
    } else {
        None
    }
}

pub(super) fn restore_keyboard_enhancement_stack() {
    let _ = execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        DisableModifyOtherKeys
    );
}

pub(super) fn reset_keyboard_reporting_after_exit() {
    let _ = execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        ResetKeyboardEnhancementFlags,
        DisableModifyOtherKeys
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetKeyboardEnhancementFlags;

impl Command for ResetKeyboardEnhancementFlags {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[<u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "keyboard enhancement reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableModifyOtherKeys;

impl Command for EnableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;2m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "modifyOtherKeys enable is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableModifyOtherKeys;

impl Command for DisableModifyOtherKeys {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[>4;0m")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "modifyOtherKeys reset is not implemented for the legacy Windows API",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::DisableModifyOtherKeys;
    use super::EnableModifyOtherKeys;
    use super::ResetKeyboardEnhancementFlags;
    use super::cmux_session_detected;
    use super::direct_terminal_all_keys_supported;
    use super::keyboard_enhancement_disabled_for;
    use super::keyboard_enhancement_flags;
    use super::parse_bool_env;
    use super::parse_tmux_bool;
    use super::should_enable_all_keys;
    use super::tmux_session_detected;
    use super::tmux_should_enable_modify_other_keys_for;
    use super::vscode_terminal_detected;
    use codex_terminal_detection::TerminalName;
    use crossterm::Command;
    use crossterm::event::PushKeyboardEnhancementFlags;
    use pretty_assertions::assert_eq;

    fn ansi_for(command: impl Command) -> String {
        let mut out = String::new();
        command.write_ansi(&mut out).unwrap();
        out
    }

    #[test]
    fn keyboard_enhancement_suppresses_release_reporting_for_iterm() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Iterm2,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ false,
            ))),
            "\x1b[>5u"
        );
    }

    #[test]
    fn keyboard_enhancement_suppresses_release_reporting_for_ghostty() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Ghostty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ false,
            ))),
            "\x1b[>5u"
        );
    }

    #[test]
    fn keyboard_enhancement_preserves_repeat_reporting_for_kitty() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Kitty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ false,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_preserves_repeat_reporting_for_csi_u_tmux() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Kitty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ true,
                Some("csi-u"),
                /*realtime_voice_enabled*/ false,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_preserves_shift_enter_for_xterm_tmux() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Ghostty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ true,
                Some("xterm"),
                /*realtime_voice_enabled*/ false,
            ))),
            "\x1b[>5u"
        );
    }

    #[test]
    fn keyboard_enhancement_preserves_repeat_reporting_for_unknown_terminals() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Unknown,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ false,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_reports_all_keys_for_live_direct_iterm() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Iterm2,
                /*all_keys_supported*/ true,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>15u"
        );
    }

    #[test]
    fn keyboard_enhancement_reports_releases_for_live_voice_on_kitty() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Kitty,
                /*all_keys_supported*/ true,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>15u"
        );
    }

    #[test]
    fn keyboard_enhancement_does_not_trust_term_only_kitty_for_live_voice() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Kitty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_reports_all_keys_for_live_direct_ghostty() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Ghostty,
                /*all_keys_supported*/ true,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>15u"
        );
    }

    #[test]
    fn keyboard_enhancement_does_not_force_all_keys_for_live_voice_on_unknown_terminal() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Unknown,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_does_not_treat_unqueryable_tmux_as_direct_kitty() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Kitty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ true,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_keeps_live_voice_on_safe_csi_u_tmux_fallback() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Ghostty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ true,
                Some("csi-u"),
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_does_not_force_all_keys_for_live_voice_on_xterm_tmux() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Iterm2,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ true,
                Some("xterm"),
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn keyboard_enhancement_keeps_cmux_live_voice_on_safe_fallback() {
        assert_eq!(
            ansi_for(PushKeyboardEnhancementFlags(keyboard_enhancement_flags(
                TerminalName::Ghostty,
                /*all_keys_supported*/ false,
                /*running_in_tmux_session*/ false,
                /*tmux_extended_keys_format*/ None,
                /*realtime_voice_enabled*/ true,
            ))),
            "\x1b[>7u"
        );
    }

    #[test]
    fn direct_terminal_all_keys_requires_direct_transport() {
        assert!(direct_terminal_all_keys_supported(
            TerminalName::Ghostty,
            /*kitty_terminal_protocol_supported*/ false,
            /*running_in_tmux_session*/ false,
            /*running_in_cmux_session*/ false,
        ));
        assert!(direct_terminal_all_keys_supported(
            TerminalName::Iterm2,
            /*kitty_terminal_protocol_supported*/ false,
            /*running_in_tmux_session*/ false,
            /*running_in_cmux_session*/ false,
        ));
        assert!(direct_terminal_all_keys_supported(
            TerminalName::Kitty,
            /*kitty_terminal_protocol_supported*/ true,
            /*running_in_tmux_session*/ false,
            /*running_in_cmux_session*/ false,
        ));
        assert!(!direct_terminal_all_keys_supported(
            TerminalName::Ghostty,
            /*kitty_terminal_protocol_supported*/ false,
            /*running_in_tmux_session*/ true,
            /*running_in_cmux_session*/ false,
        ));
        assert!(!direct_terminal_all_keys_supported(
            TerminalName::Ghostty,
            /*kitty_terminal_protocol_supported*/ false,
            /*running_in_tmux_session*/ false,
            /*running_in_cmux_session*/ true,
        ));
        assert!(!direct_terminal_all_keys_supported(
            TerminalName::Kitty,
            /*kitty_terminal_protocol_supported*/ false,
            /*running_in_tmux_session*/ false,
            /*running_in_cmux_session*/ false,
        ));
    }

    #[test]
    fn right_option_monitor_keeps_terminal_text_on_safe_encoding() {
        assert!(!should_enable_all_keys(
            TerminalName::Ghostty,
            /*kitty_terminal_protocol_supported*/ false,
            /*running_in_tmux_session*/ false,
            /*running_in_cmux_session*/ false,
            /*right_option_monitor_enabled*/ true,
        ));
        assert!(should_enable_all_keys(
            TerminalName::Ghostty,
            /*kitty_terminal_protocol_supported*/ false,
            /*running_in_tmux_session*/ false,
            /*running_in_cmux_session*/ false,
            /*right_option_monitor_enabled*/ false,
        ));
    }

    #[test]
    fn cmux_session_detection_requires_a_nonempty_id() {
        assert!(cmux_session_detected(Some("workspace"), None));
        assert!(cmux_session_detected(None, Some("surface")));
        assert!(!cmux_session_detected(Some("  "), Some("")));
        assert!(!cmux_session_detected(None, None));
    }

    #[test]
    fn keyboard_enhancement_env_flag_parses_common_values() {
        assert_eq!(parse_bool_env(Some("1")), Some(true));
        assert_eq!(parse_bool_env(Some("true")), Some(true));
        assert_eq!(parse_bool_env(Some("YES")), Some(true));
        assert_eq!(parse_bool_env(Some("0")), Some(false));
        assert_eq!(parse_bool_env(Some("false")), Some(false));
        assert_eq!(parse_bool_env(Some("NO")), Some(false));
        assert_eq!(parse_bool_env(Some("unexpected")), None);
        assert_eq!(parse_bool_env(/*value*/ None), None);
    }

    #[test]
    fn tmux_extended_keys_values_parse_enabled_state() {
        assert_eq!(parse_tmux_bool("on"), Some(true));
        assert_eq!(parse_tmux_bool("always"), Some(true));
        assert_eq!(parse_tmux_bool("TRUE"), Some(true));
        assert_eq!(parse_tmux_bool("1"), Some(true));
        assert_eq!(parse_tmux_bool("off"), Some(false));
        assert_eq!(parse_tmux_bool("FALSE"), Some(false));
        assert_eq!(parse_tmux_bool("0"), Some(false));
        assert_eq!(parse_tmux_bool("auto"), None);
    }

    #[test]
    fn keyboard_enhancement_auto_disables_for_vscode_in_wsl() {
        assert!(keyboard_enhancement_disabled_for(
            /*disable_env*/ None, /*is_wsl*/ true, /*is_vscode_terminal*/ true
        ));
    }

    #[test]
    fn keyboard_enhancement_auto_disable_requires_wsl_and_vscode() {
        assert!(!keyboard_enhancement_disabled_for(
            /*disable_env*/ None, /*is_wsl*/ true, /*is_vscode_terminal*/ false
        ));
        assert!(!keyboard_enhancement_disabled_for(
            /*disable_env*/ None, /*is_wsl*/ false, /*is_vscode_terminal*/ true
        ));
    }

    #[test]
    fn keyboard_enhancement_env_flag_overrides_auto_detection() {
        assert!(!keyboard_enhancement_disabled_for(
            Some("0"),
            /*is_wsl*/ true,
            /*is_vscode_terminal*/ true
        ));
        assert!(keyboard_enhancement_disabled_for(
            Some("1"),
            /*is_wsl*/ false,
            /*is_vscode_terminal*/ false
        ));
    }

    #[test]
    fn vscode_terminal_detection_uses_linux_and_windows_term_program() {
        assert!(vscode_terminal_detected(
            Some("vscode"),
            /*windows_term_program*/ None
        ));
        assert!(vscode_terminal_detected(
            /*linux_term_program*/ None,
            Some("vscode")
        ));
        assert!(!vscode_terminal_detected(
            /*linux_term_program*/ None,
            Some("WindowsTerminal")
        ));
        assert!(!vscode_terminal_detected(
            /*linux_term_program*/ None, /*windows_term_program*/ None
        ));
    }

    #[test]
    fn tmux_session_detection_accepts_tmux_or_tmux_pane() {
        assert!(tmux_session_detected(
            Some("/tmp/tmux-501/default,1,0"),
            /*tmux_pane*/ None
        ));
        assert!(tmux_session_detected(/*tmux*/ None, Some("%0")));
        assert!(!tmux_session_detected(
            /*tmux*/ None, /*tmux_pane*/ None
        ));
    }

    #[test]
    fn tmux_modify_other_keys_only_requests_confirmed_csi_u_format() {
        assert!(tmux_should_enable_modify_other_keys_for(
            /*running_in_tmux_session*/ true,
            Some("csi-u")
        ));
        assert!(!tmux_should_enable_modify_other_keys_for(
            /*running_in_tmux_session*/ true, /*extended_keys_format*/ None
        ));
        assert!(!tmux_should_enable_modify_other_keys_for(
            /*running_in_tmux_session*/ true,
            Some("xterm")
        ));
        assert!(!tmux_should_enable_modify_other_keys_for(
            /*running_in_tmux_session*/ true,
            Some("")
        ));
        assert!(!tmux_should_enable_modify_other_keys_for(
            /*running_in_tmux_session*/ false,
            Some("csi-u")
        ));
    }

    #[test]
    fn reset_keyboard_enhancement_flags_clears_all_pushed_levels() {
        assert_eq!(ansi_for(ResetKeyboardEnhancementFlags), "\x1b[<u");
    }

    #[test]
    fn enable_modify_other_keys_requests_xterm_keyboard_reporting() {
        assert_eq!(ansi_for(EnableModifyOtherKeys), "\x1b[>4;2m");
    }

    #[test]
    fn disable_modify_other_keys_resets_xterm_keyboard_reporting() {
        assert_eq!(ansi_for(DisableModifyOtherKeys), "\x1b[>4;0m");
    }
}
