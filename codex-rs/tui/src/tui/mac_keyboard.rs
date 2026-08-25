//! macOS keyboard state that is not reliably representable by a terminal protocol.
//!
//! macOS terminals do not have a portable way to report a modifier-only key such as
//! Right Option without enabling CSI-u's all-keys mode. Some remote desktop clients
//! corrupt ordinary text while that mode is enabled, so the TUI observes only the
//! Right Option state through CoreGraphics and leaves text input on the compatible
//! terminal encoding.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(target_os = "macos")]
use std::ffi::OsString;
#[cfg(target_os = "macos")]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::io::Read;
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::ptr::null;
#[cfg(target_os = "macos")]
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::thread::JoinHandle;
#[cfg(target_os = "macos")]
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyEventState;
use crossterm::event::KeyModifiers;
use crossterm::event::ModifierKeyCode;
use tokio::sync::broadcast;

// kVK_RightOption from Carbon HIToolbox.
const RIGHT_OPTION_KEY_CODE: u16 = 61;
// kVK_Option from Carbon HIToolbox.
const LEFT_OPTION_KEY_CODE: u16 = 58;
const POLL_INTERVAL: Duration = Duration::from_millis(8);
#[cfg(target_os = "macos")]
const CMUX_SURFACE_ID_ENV_VAR: &str = "CMUX_SURFACE_ID";
#[cfg(target_os = "macos")]
const CMUX_BUNDLED_CLI_PATH_ENV_VAR: &str = "CMUX_BUNDLED_CLI_PATH";
#[cfg(target_os = "macos")]
const CMUX_CODEX_HOOK_CMUX_BIN_ENV_VAR: &str = "CMUX_CODEX_HOOK_CMUX_BIN";
#[cfg(target_os = "macos")]
const CMUX_IDENTIFY_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(target_os = "macos")]
const CMUX_IDENTIFY_MAX_OUTPUT_BYTES: u64 = 64 * 1024;
#[cfg(target_os = "macos")]
const CMUX_FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(50);
// kCGEventSourceStateHIDSystemState from CoreGraphics.
const HID_SYSTEM_STATE: u32 = 1;
// kCGEventFlagsChanged from CoreGraphics.
const FLAGS_CHANGED_EVENT: u32 = 12;
// kCGKeyboardEventKeycode from CoreGraphics.
const KEYBOARD_EVENT_KEYCODE_FIELD: u32 = 9;
// kCGEventFlagMaskAlternate from CoreGraphics.
const ALTERNATE_EVENT_FLAG: u64 = 1 << 19;

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceKeyState(state_id: u32, key: u16) -> bool;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: Option<
            unsafe extern "C" fn(
                proxy: *mut c_void,
                event_type: u32,
                event: *mut c_void,
                user_info: *mut c_void,
            ) -> *mut c_void,
        >,
        user_info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetFlags(event: *mut c_void) -> u64;
    fn CGEventGetIntegerValueField(event: *mut c_void, field: u32) -> i64;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *mut c_void,
        order: isize,
    ) -> *mut c_void;
    fn CFMachPortInvalidate(port: *mut c_void);
    fn CFRelease(value: *const c_void);
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRemoveSource(run_loop: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRunInMode(
        mode: *const c_void,
        seconds: f64,
        return_after_source_handled: bool,
    ) -> i32;
    fn CFRunLoopSourceInvalidate(source: *mut c_void);
    static kCFRunLoopDefaultMode: *const c_void;
}

/// Observes macOS Right Option without changing terminal text encoding.
pub(crate) struct MacRightOptionMonitor {
    events_tx: broadcast::Sender<KeyEvent>,
    paused: Arc<AtomicBool>,
    focused: Arc<AtomicBool>,
    cmux_focus_probe: Option<CmuxFocusProbe>,
    release_pending: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Identifies the cmux surface that owns a Codex process so native modifier events can be
/// delivered only to the surface currently receiving keyboard input.
#[derive(Clone)]
pub(crate) struct CmuxFocusProbe {
    #[cfg(target_os = "macos")]
    state: Arc<CmuxFocusState>,
}

impl CmuxFocusProbe {
    pub(crate) fn from_environment() -> Option<Self> {
        #[cfg(not(target_os = "macos"))]
        {
            None
        }

        #[cfg(target_os = "macos")]
        {
            let surface_id = nonempty_env(CMUX_SURFACE_ID_ENV_VAR)?;
            let focused = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let focused_for_thread = Arc::clone(&focused);
            let stop_for_thread = Arc::clone(&stop);
            let thread = std::thread::Builder::new()
                .name("codex-cmux-focus-probe".to_string())
                .spawn(move || {
                    while !stop_for_thread.load(Ordering::Relaxed) {
                        let is_focused = run_cmux_identify()
                            .and_then(|output| cmux_identify_focus_matches(&output, &surface_id))
                            .unwrap_or(false);
                        focused_for_thread.store(is_focused, Ordering::Relaxed);
                        std::thread::sleep(CMUX_FOCUS_POLL_INTERVAL);
                    }
                })
                .map_err(|error| {
                    tracing::warn!(
                        error = %error,
                        "failed to start cmux focus probe; native push-to-talk will remain disabled"
                    );
                })
                .ok();
            Some(Self {
                state: Arc::new(CmuxFocusState {
                    focused,
                    stop,
                    thread: Mutex::new(thread),
                }),
            })
        }
    }

    pub(crate) fn is_focused(&self) -> bool {
        #[cfg(not(target_os = "macos"))]
        {
            true
        }

        #[cfg(target_os = "macos")]
        {
            self.state.focused.load(Ordering::Relaxed)
        }
    }
}

#[cfg(target_os = "macos")]
struct CmuxFocusState {
    focused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

#[cfg(target_os = "macos")]
impl Drop for CmuxFocusState {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self
            .thread
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = thread.join();
        }
    }
}

impl MacRightOptionMonitor {
    pub(crate) fn new(enabled: bool) -> Option<Self> {
        if !enabled {
            return None;
        }

        #[cfg(not(target_os = "macos"))]
        {
            None
        }

        #[cfg(target_os = "macos")]
        {
            let (events_tx, _) = broadcast::channel(8);
            let paused = Arc::new(AtomicBool::new(false));
            let focused = Arc::new(AtomicBool::new(true));
            let cmux_focus_probe = CmuxFocusProbe::from_environment();
            let release_pending = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let paused_for_thread = Arc::clone(&paused);
            let focused_for_thread = Arc::clone(&focused);
            let stop_for_thread = Arc::clone(&stop);
            let events_tx_for_thread = events_tx.clone();
            let thread = match std::thread::Builder::new()
                .name("codex-right-option-monitor".to_string())
                .spawn(move || {
                    tracing::debug!(
                        key_code = RIGHT_OPTION_KEY_CODE,
                        "started macOS Right Option monitor"
                    );
                    if !run_flags_changed_monitor(
                        events_tx_for_thread.clone(),
                        paused_for_thread.clone(),
                        focused_for_thread.clone(),
                        stop_for_thread.clone(),
                    ) {
                        tracing::debug!(
                            "falling back to macOS Right Option state polling; the global modifier event tap was unavailable"
                        );
                        poll_right_option_state(
                            events_tx_for_thread,
                            paused_for_thread,
                            focused_for_thread,
                            stop_for_thread,
                        );
                    }
                }) {
                Ok(thread) => thread,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "failed to start macOS Right Option monitor; using terminal keyboard reporting"
                    );
                    return None;
                }
            };

            Some(Self {
                events_tx,
                paused,
                focused,
                cmux_focus_probe,
                release_pending,
                stop,
                thread: Some(thread),
            })
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<KeyEvent> {
        self.events_tx.subscribe()
    }

    pub(crate) fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        self.release_pending.store(true, Ordering::Relaxed);
    }

    pub(crate) fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    pub(crate) fn focus_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.focused)
    }

    pub(crate) fn cmux_focus_probe(&self) -> Option<CmuxFocusProbe> {
        self.cmux_focus_probe.clone()
    }

    pub(crate) fn pause_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.paused)
    }

    pub(crate) fn release_pending_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.release_pending)
    }

    pub(crate) fn release_event() -> KeyEvent {
        right_option_event(KeyEventKind::Release)
    }
}

#[cfg(target_os = "macos")]
unsafe fn right_option_is_pressed() -> bool {
    // Use only HID state so a remote keyboard or terminal-injected modifier cannot activate the
    // physical Mac push-to-talk binding.
    unsafe { option_key_is_pressed(RIGHT_OPTION_KEY_CODE) }
}

#[cfg(target_os = "macos")]
unsafe fn option_key_is_pressed(key_code: u16) -> bool {
    unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, key_code) }
}

#[cfg(target_os = "macos")]
fn poll_right_option_state(
    events_tx: broadcast::Sender<KeyEvent>,
    paused: Arc<AtomicBool>,
    focused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut last_pressed = false;
    while !stop.load(Ordering::Relaxed) {
        if paused.load(Ordering::Relaxed) || !focused.load(Ordering::Relaxed) {
            // Do not replay a stale press after an external interactive program returns. A key
            // held through the transition will be reported as a fresh press when polling resumes.
            last_pressed = false;
        } else {
            let pressed = unsafe { right_option_is_pressed() };
            if pressed != last_pressed {
                last_pressed = pressed;
                let kind = if pressed {
                    KeyEventKind::Press
                } else {
                    KeyEventKind::Release
                };
                let _ = events_tx.send(right_option_event(kind));
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(target_os = "macos")]
struct FlagsChangedContext {
    events_tx: broadcast::Sender<KeyEvent>,
    paused: Arc<AtomicBool>,
    focused: Arc<AtomicBool>,
    right_pressed: bool,
    left_pressed: bool,
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn flags_changed_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    user_info: *mut c_void,
) -> *mut c_void {
    if event_type != FLAGS_CHANGED_EVENT || user_info.is_null() || event.is_null() {
        return event;
    }

    let context = unsafe { &mut *(user_info as *mut FlagsChangedContext) };
    let key_code = unsafe { CGEventGetIntegerValueField(event, KEYBOARD_EVENT_KEYCODE_FIELD) };
    let flags = unsafe { CGEventGetFlags(event) };
    let Some((key_code, pressed)) = update_option_state_from_event(
        key_code,
        flags,
        &mut context.right_pressed,
        &mut context.left_pressed,
    ) else {
        return event;
    };

    if key_code != RIGHT_OPTION_KEY_CODE {
        return event;
    }

    if context.paused.load(Ordering::Relaxed) || !context.focused.load(Ordering::Relaxed) {
        context.right_pressed = false;
        return event;
    }

    let kind = if pressed {
        KeyEventKind::Press
    } else {
        KeyEventKind::Release
    };
    let _ = context.events_tx.send(right_option_event(kind));

    event
}

#[cfg(target_os = "macos")]
fn run_flags_changed_monitor(
    events_tx: broadcast::Sender<KeyEvent>,
    paused: Arc<AtomicBool>,
    focused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> bool {
    // Listen before session-level event injection so this path follows the physical Mac
    // keyboard only. AstroPad's remote keyboard does not need to produce a Right Option event;
    // its ordinary text path must remain entirely on the compatible terminal protocol.
    const HID_EVENT_TAP: u32 = 0;
    const HEAD_INSERT_EVENT_TAP: u32 = 0;
    const LISTEN_ONLY_EVENT_TAP: u32 = 1;
    let event_mask = 1u64 << FLAGS_CHANGED_EVENT;
    let mut context = Box::new(FlagsChangedContext {
        events_tx,
        paused,
        focused,
        right_pressed: unsafe { option_key_is_pressed(RIGHT_OPTION_KEY_CODE) },
        left_pressed: unsafe { option_key_is_pressed(LEFT_OPTION_KEY_CODE) },
    });
    let context_ptr = (&mut *context) as *mut FlagsChangedContext as *mut c_void;
    let event_tap = unsafe {
        CGEventTapCreate(
            HID_EVENT_TAP,
            HEAD_INSERT_EVENT_TAP,
            LISTEN_ONLY_EVENT_TAP,
            event_mask,
            Some(flags_changed_callback),
            context_ptr,
        )
    };
    if event_tap.is_null() {
        tracing::warn!(
            "macOS global modifier event tap could not be created; physical Right Option polling will be used. Grant Codex Accessibility/Input Monitoring permission if the physical Right Option key is not detected"
        );
        return false;
    }

    let run_loop = unsafe { CFRunLoopGetCurrent() };
    let source = unsafe { CFMachPortCreateRunLoopSource(null(), event_tap, 0) };
    if source.is_null() {
        unsafe {
            CFMachPortInvalidate(event_tap);
            CFRelease(event_tap);
        }
        tracing::warn!("failed to create the macOS modifier event tap run-loop source");
        return false;
    }

    unsafe {
        CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode);
        CGEventTapEnable(event_tap, true);
    }

    while !stop.load(Ordering::Relaxed) {
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, true);
        }
    }

    unsafe {
        CFRunLoopRemoveSource(run_loop, source, kCFRunLoopDefaultMode);
        CFRunLoopSourceInvalidate(source);
        CFMachPortInvalidate(event_tap);
        CFRelease(source);
        CFRelease(event_tap);
    }
    drop(context);
    true
}

fn update_option_state_from_event(
    key_code: i64,
    flags: u64,
    right_pressed: &mut bool,
    left_pressed: &mut bool,
) -> Option<(u16, bool)> {
    match key_code {
        code if code == i64::from(RIGHT_OPTION_KEY_CODE) => {
            let pressed = if *left_pressed {
                !*right_pressed
            } else {
                flags & ALTERNATE_EVENT_FLAG != 0
            };
            *right_pressed = pressed;
            Some((RIGHT_OPTION_KEY_CODE, pressed))
        }
        code if code == i64::from(LEFT_OPTION_KEY_CODE) => {
            let pressed = if *right_pressed {
                !*left_pressed
            } else {
                flags & ALTERNATE_EVENT_FLAG != 0
            };
            *left_pressed = pressed;
            Some((LEFT_OPTION_KEY_CODE, pressed))
        }
        _ => None,
    }
}

#[cfg(test)]
fn right_option_state_from_event(key_code: i64, flags: u64) -> Option<bool> {
    let mut right_pressed = false;
    let mut left_pressed = false;
    update_option_state_from_event(key_code, flags, &mut right_pressed, &mut left_pressed)
        .and_then(|(key_code, pressed)| (key_code == RIGHT_OPTION_KEY_CODE).then_some(pressed))
}

impl Drop for MacRightOptionMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn right_option_event(kind: KeyEventKind) -> KeyEvent {
    KeyEvent {
        code: KeyCode::Modifier(ModifierKeyCode::RightAlt),
        modifiers: KeyModifiers::NONE,
        kind,
        state: KeyEventState::NONE,
    }
}

#[cfg(target_os = "macos")]
fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(target_os = "macos")]
fn run_cmux_identify() -> Option<Vec<u8>> {
    let cli_path = std::env::var_os(CMUX_BUNDLED_CLI_PATH_ENV_VAR)
        .or_else(|| std::env::var_os(CMUX_CODEX_HOOK_CMUX_BIN_ENV_VAR))
        .unwrap_or_else(|| OsString::from("cmux"));
    let mut child = Command::new(cli_path)
        .args(["--json", "--id-format", "uuids", "identify", "--no-caller"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take(CMUX_IDENTIFY_MAX_OUTPUT_BYTES.saturating_add(1))
            .read_to_end(&mut output)
            .ok()?;
        Some(output)
    });
    let deadline = Instant::now() + CMUX_IDENTIFY_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = reader.join().ok().flatten()?;
    let status = status?;
    if !status.success() || output.len() > CMUX_IDENTIFY_MAX_OUTPUT_BYTES as usize {
        return None;
    }
    Some(output)
}

fn cmux_identify_focus_matches(output: &[u8], surface_id: &str) -> Option<bool> {
    let document: serde_json::Value = serde_json::from_slice(output).ok()?;
    let focused = document.get("focused")?;
    let focused_surface = focused
        .get("surface_id")
        .or_else(|| focused.get("surface_ref"))
        .or_else(|| focused.get("panel_id"))
        .or_else(|| focused.get("panel_ref"))?
        .as_str()?;
    Some(focused_surface == surface_id)
}

#[cfg(test)]
mod tests {
    use super::cmux_identify_focus_matches;
    use super::right_option_event;
    use super::right_option_state_from_event;
    use super::update_option_state_from_event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::event::ModifierKeyCode;
    use pretty_assertions::assert_eq;

    #[test]
    fn right_option_monitor_emits_terminal_compatible_press_and_release_events() {
        assert_eq!(
            right_option_event(KeyEventKind::Press),
            crossterm::event::KeyEvent {
                code: KeyCode::Modifier(ModifierKeyCode::RightAlt),
                modifiers: crossterm::event::KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: crossterm::event::KeyEventState::NONE,
            }
        );
        assert_eq!(
            right_option_event(KeyEventKind::Release).kind,
            KeyEventKind::Release
        );
    }

    #[test]
    fn flags_changed_events_identify_right_option_state() {
        assert_eq!(
            right_option_state_from_event(
                i64::from(super::RIGHT_OPTION_KEY_CODE),
                super::ALTERNATE_EVENT_FLAG,
            ),
            Some(true)
        );
        assert_eq!(
            right_option_state_from_event(i64::from(super::RIGHT_OPTION_KEY_CODE), 0),
            Some(false)
        );
        assert_eq!(
            right_option_state_from_event(58, super::ALTERNATE_EVENT_FLAG),
            None
        );
    }

    #[test]
    fn overlapping_option_flags_preserve_right_option_release() {
        let mut right_pressed = false;
        let mut left_pressed = false;

        assert_eq!(
            update_option_state_from_event(
                i64::from(super::LEFT_OPTION_KEY_CODE),
                super::ALTERNATE_EVENT_FLAG,
                &mut right_pressed,
                &mut left_pressed,
            ),
            Some((super::LEFT_OPTION_KEY_CODE, true))
        );
        assert_eq!(
            update_option_state_from_event(
                i64::from(super::RIGHT_OPTION_KEY_CODE),
                super::ALTERNATE_EVENT_FLAG,
                &mut right_pressed,
                &mut left_pressed,
            ),
            Some((super::RIGHT_OPTION_KEY_CODE, true))
        );
        assert_eq!(
            update_option_state_from_event(
                i64::from(super::RIGHT_OPTION_KEY_CODE),
                super::ALTERNATE_EVENT_FLAG,
                &mut right_pressed,
                &mut left_pressed,
            ),
            Some((super::RIGHT_OPTION_KEY_CODE, false))
        );
    }

    #[test]
    fn cmux_focus_matches_only_the_current_surface() {
        let identify = br#"{
            "focused": {
                "surface_id": "surface-active",
                "surface_type": "terminal"
            }
        }"#;

        assert_eq!(
            cmux_identify_focus_matches(identify, "surface-active"),
            Some(true)
        );
        assert_eq!(
            cmux_identify_focus_matches(identify, "surface-background"),
            Some(false)
        );

        let legacy_identify = br#"{
            "focused": {
                "surface_ref": "surface-active"
            }
        }"#;
        assert_eq!(
            cmux_identify_focus_matches(legacy_identify, "surface-active"),
            Some(true)
        );
        let panel_identify = br#"{
            "focused": {
                "panel_ref": "surface-active"
            }
        }"#;
        assert_eq!(
            cmux_identify_focus_matches(panel_identify, "surface-active"),
            Some(true)
        );
        assert_eq!(cmux_identify_focus_matches(b"{}", "surface-active"), None);
    }
}
