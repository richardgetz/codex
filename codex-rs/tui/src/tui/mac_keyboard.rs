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
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::ptr::null;

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
    release_pending: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
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

#[cfg(test)]
mod tests {
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
}
