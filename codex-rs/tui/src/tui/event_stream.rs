//! Event stream plumbing for the TUI.
//!
//! - [`EventBroker`] holds the shared crossterm stream so multiple callers reuse the same
//!   input source and can drop/recreate it on pause/resume without rebuilding consumers.
//! - [`TuiEventStream`] wraps a draw event subscription plus the shared [`EventBroker`] and maps crossterm
//!   events into [`TuiEvent`].
//! - [`EventSource`] abstracts the underlying event producer; the real implementation is
//!   [`CrosstermEventSource`] and tests can swap in [`FakeEventSource`].
//!
//! The motivation for dropping/recreating the crossterm event stream is to enable the TUI to fully relinquish stdin.
//! If the stream is not dropped, it will continue to read from stdin even if it is not actively being polled
//! (due to how crossterm's EventStream is implemented), potentially stealing input from other processes reading stdin,
//! like terminal text editors. This race can cause missed input or capturing terminal query responses (for example, OSC palette/size queries)
//! that the other process expects to read. Stopping polling, instead of dropping the stream, is only sufficient when the
//! pause happens before the stream enters a pending state; otherwise the crossterm reader thread may keep reading
//! from stdin, so the safer approach is to drop and recreate the event stream when we need to hand off the terminal.
//!
//! See https://ratatui.rs/recipes/apps/spawn-vim/ and https://www.reddit.com/r/rust/comments/1f3o33u/myterious_crossterm_input_after_running_vim for more details.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use super::TuiEvent;

/// Result type produced by an event source.
pub type EventResult = std::io::Result<Event>;

/// Abstraction over a source of terminal events. Allows swapping in a fake for tests.
/// Value in production is [`CrosstermEventSource`].
pub trait EventSource: Send + 'static {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<EventResult>>;
}

/// Shared crossterm input state for all [`TuiEventStream`] instances. A single crossterm EventStream
/// is reused so all streams still see the same input source.
///
/// This intermediate layer enables dropping/recreating the underlying EventStream (pause/resume) without rebuilding consumers.
pub struct EventBroker<S: EventSource = CrosstermEventSource> {
    state: Mutex<EventBrokerState<S>>,
    resume_events_tx: watch::Sender<()>,
}

/// Tracks state of underlying [`EventSource`].
enum EventBrokerState<S: EventSource> {
    Paused,     // Underlying event source (i.e., crossterm EventStream) dropped
    Start,      // A new event source will be created on next poll
    Running(S), // Event source is currently running
}

impl<S: EventSource + Default> EventBrokerState<S> {
    /// Return the running event source, starting it if needed; None when paused.
    fn active_event_source_mut(&mut self) -> Option<&mut S> {
        match self {
            EventBrokerState::Paused => None,
            EventBrokerState::Start => {
                *self = EventBrokerState::Running(S::default());
                match self {
                    EventBrokerState::Running(events) => Some(events),
                    EventBrokerState::Paused | EventBrokerState::Start => unreachable!(),
                }
            }
            EventBrokerState::Running(events) => Some(events),
        }
    }
}

impl<S: EventSource + Default> EventBroker<S> {
    pub fn new() -> Self {
        let (resume_events_tx, _resume_events_rx) = watch::channel(());
        Self {
            state: Mutex::new(EventBrokerState::Start),
            resume_events_tx,
        }
    }

    /// Drop the underlying event source
    pub fn pause_events(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = EventBrokerState::Paused;
    }

    /// Create a new instance of the underlying event source
    pub fn resume_events(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = EventBrokerState::Start;
        let _ = self.resume_events_tx.send(());
    }

    /// Subscribe to a notification that fires whenever [`Self::resume_events`] is called.
    ///
    /// This is used to wake `poll_crossterm_event` when it is paused and waiting for the
    /// underlying crossterm stream to be recreated.
    pub fn resume_events_rx(&self) -> watch::Receiver<()> {
        self.resume_events_tx.subscribe()
    }
}

/// Real crossterm-backed event source.
pub struct CrosstermEventSource(pub crossterm::event::EventStream);

impl Default for CrosstermEventSource {
    fn default() -> Self {
        Self(crossterm::event::EventStream::new())
    }
}

impl EventSource for CrosstermEventSource {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<EventResult>> {
        // Crossterm's Windows backend expects Win32 input records. If VT input is inherited or
        // restored by another console client, navigation keys arrive as literal escape bytes.
        #[cfg(windows)]
        let _ = super::windows_console::ensure_input_record_mode();

        let result = Pin::new(&mut self.get_mut().0).poll_next(cx);

        // EventStream starts its blocking reader before returning Pending, so reassert the mode
        // after that transition as well.
        #[cfg(windows)]
        if result.is_pending() {
            let _ = super::windows_console::ensure_input_record_mode();
        }

        result
    }
}

/// TuiEventStream is a struct for reading TUI events (draws and user input).
/// Each instance has its own draw subscription (the draw channel is broadcast, so
/// multiple receivers are fine), while crossterm input is funneled through a
/// single shared [`EventBroker`] because crossterm uses a global stdin reader and
/// does not support fan-out. Multiple TuiEventStream instances can exist during the app lifetime
/// (for nested or sequential screens), but only one should be polled at a time,
/// otherwise one instance can consume ("steal") input events and the other will miss them.
pub struct TuiEventStream<S: EventSource + Default + Unpin = CrosstermEventSource> {
    broker: Arc<EventBroker<S>>,
    draw_stream: BroadcastStream<()>,
    mac_keyboard_stream: Option<BroadcastStream<KeyEvent>>,
    mac_keyboard_focused: Option<Arc<AtomicBool>>,
    mac_keyboard_cmux_focus: Option<super::mac_keyboard::CmuxFocusProbe>,
    last_cmux_focused: Option<bool>,
    mac_keyboard_paused: Option<Arc<AtomicBool>>,
    mac_keyboard_release_pending: Option<Arc<AtomicBool>>,
    resume_stream: WatchStream<()>,
    terminal_focused: Arc<AtomicBool>,
    poll_draw_first: bool,
    #[cfg(unix)]
    suspend_context: crate::tui::job_control::SuspendContext,
    #[cfg(unix)]
    alt_screen_active: Arc<AtomicBool>,
}

impl<S: EventSource + Default + Unpin> TuiEventStream<S> {
    pub fn new(
        broker: Arc<EventBroker<S>>,
        draw_rx: broadcast::Receiver<()>,
        mac_keyboard_rx: Option<broadcast::Receiver<KeyEvent>>,
        mac_keyboard_focused: Option<Arc<AtomicBool>>,
        mac_keyboard_cmux_focus: Option<super::mac_keyboard::CmuxFocusProbe>,
        mac_keyboard_paused: Option<Arc<AtomicBool>>,
        mac_keyboard_release_pending: Option<Arc<AtomicBool>>,
        terminal_focused: Arc<AtomicBool>,
        #[cfg(unix)] suspend_context: crate::tui::job_control::SuspendContext,
        #[cfg(unix)] alt_screen_active: Arc<AtomicBool>,
    ) -> Self {
        let resume_stream = WatchStream::from_changes(broker.resume_events_rx());
        Self {
            broker,
            draw_stream: BroadcastStream::new(draw_rx),
            mac_keyboard_stream: mac_keyboard_rx.map(BroadcastStream::new),
            mac_keyboard_focused,
            mac_keyboard_cmux_focus,
            last_cmux_focused: None,
            mac_keyboard_paused,
            mac_keyboard_release_pending,
            resume_stream,
            terminal_focused,
            poll_draw_first: false,
            #[cfg(unix)]
            suspend_context,
            #[cfg(unix)]
            alt_screen_active,
        }
    }

    /// Poll the shared crossterm stream for the next mapped `TuiEvent`.
    ///
    /// This skips events we don't use (mouse events, etc.) and keeps polling until it yields
    /// a mapped event, hits `Pending`, or sees EOF/error. When the broker is paused, it drops
    /// the underlying stream and returns `Pending` to fully release stdin.
    pub fn poll_crossterm_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        // Some crossterm events map to None (e.g. FocusLost, mouse); loop so we keep polling
        // until we return a mapped event, hit Pending, or see EOF/error.
        loop {
            let poll_result = {
                let mut state = self
                    .broker
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let events = match state.active_event_source_mut() {
                    Some(events) => events,
                    None => {
                        drop(state);
                        // Poll resume_stream so resume_events wakes a stream paused here
                        match Pin::new(&mut self.resume_stream).poll_next(cx) {
                            Poll::Ready(Some(())) => continue,
                            Poll::Ready(None) => return Poll::Ready(None),
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                };
                match Pin::new(events).poll_next(cx) {
                    Poll::Ready(Some(Ok(event))) => Some(event),
                    Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                        *state = EventBrokerState::Start;
                        return Poll::Ready(None);
                    }
                    Poll::Pending => {
                        drop(state);
                        // Poll resume_stream so resume_events can wake us even while waiting on stdin
                        match Pin::new(&mut self.resume_stream).poll_next(cx) {
                            Poll::Ready(Some(())) => continue,
                            Poll::Ready(None) => return Poll::Ready(None),
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                }
            };

            if let Some(mapped) = poll_result.and_then(|event| self.map_crossterm_event(event)) {
                return Poll::Ready(Some(mapped));
            }
        }
    }

    /// Poll the draw broadcast stream for the next draw event. Draw events are used to trigger a redraw of the TUI.
    pub fn poll_draw_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        match Pin::new(&mut self.draw_stream).poll_next(cx) {
            Poll::Ready(Some(Ok(()))) => Poll::Ready(Some(TuiEvent::Draw)),
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {
                Poll::Ready(Some(TuiEvent::Draw))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_mac_keyboard_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        if self
            .mac_keyboard_release_pending
            .as_ref()
            .is_some_and(|pending| pending.swap(false, Ordering::Relaxed))
        {
            // A native press queued just before a terminal handoff is stale: the synthetic
            // release below supersedes it. Drain it before returning so it cannot re-open PTT.
            self.drain_mac_keyboard_events(cx);
            return Poll::Ready(Some(TuiEvent::Key(
                super::mac_keyboard::MacRightOptionMonitor::release_event(),
            )));
        }

        if self
            .mac_keyboard_focused
            .as_ref()
            .is_some_and(|focused| !focused.load(Ordering::Relaxed))
        {
            self.drain_mac_keyboard_events(cx);
            return Poll::Pending;
        }

        loop {
            let poll_result = {
                let Some(stream) = self.mac_keyboard_stream.as_mut() else {
                    return Poll::Pending;
                };
                Pin::new(&mut *stream).poll_next(cx)
            };
            match poll_result {
                Poll::Ready(Some(Ok(key_event))) => {
                    if let Some(probe) = &self.mac_keyboard_cmux_focus {
                        let focused = probe.is_focused();
                        let previous_focus = self.last_cmux_focused.replace(focused);
                        match cmux_key_event_decision(focused, previous_focus, key_event.kind) {
                            CmuxKeyEventDecision::Deliver => {}
                            CmuxKeyEventDecision::Drop => {
                                continue;
                            }
                            CmuxKeyEventDecision::Release => {
                                return Poll::Ready(Some(TuiEvent::Key(
                                    super::mac_keyboard::MacRightOptionMonitor::release_event(),
                                )));
                            }
                        }
                    }
                    return Poll::Ready(Some(TuiEvent::Key(key_event)));
                }
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => continue,
                Poll::Ready(None) => {
                    self.mac_keyboard_stream = None;
                    return Poll::Pending;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn drain_mac_keyboard_events(&mut self, cx: &mut Context<'_>) {
        let Some(stream) = self.mac_keyboard_stream.as_mut() else {
            return;
        };

        loop {
            match Pin::new(&mut *stream).poll_next(cx) {
                Poll::Ready(Some(Ok(_)))
                | Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {}
                Poll::Ready(None) => {
                    self.mac_keyboard_stream = None;
                    return;
                }
                Poll::Pending => return,
            }
        }
    }

    fn pause_mac_keyboard(&self) {
        if let Some(paused) = &self.mac_keyboard_paused {
            paused.store(true, Ordering::Relaxed);
        }
        if let Some(pending) = &self.mac_keyboard_release_pending {
            pending.store(true, Ordering::Relaxed);
        }
    }

    fn resume_mac_keyboard(&self) {
        if let Some(paused) = &self.mac_keyboard_paused {
            paused.store(false, Ordering::Relaxed);
        }
    }

    /// Map a crossterm event to a [`TuiEvent`], skipping events we don't use (mouse events, etc.).
    fn map_crossterm_event(&mut self, event: Event) -> Option<TuiEvent> {
        match event {
            Event::Key(key_event) => {
                // A few CSI-u producers encode Backspace as codepoint 8 instead of the usual
                // DEL codepoint 127. Crossterm intentionally preserves that CSI-u codepoint as
                // a character, but it is still the user's Backspace key at the TUI boundary.
                let key_event = normalize_terminal_key_event(key_event);
                if let Some(focused) = &self.mac_keyboard_focused {
                    // Some remote terminal bridges emit FocusLost without a matching
                    // FocusGained. Any key arriving through the terminal proves that input has
                    // resumed, so re-arm the native Right Option monitor here.
                    focused.store(true, Ordering::Relaxed);
                }
                #[cfg(unix)]
                if crate::tui::job_control::SUSPEND_KEY.is_press(key_event) {
                    self.pause_mac_keyboard();
                    self.broker.pause_events();
                    let suspend_result = self.suspend_context.suspend(&self.alt_screen_active);
                    self.broker.resume_events();
                    self.resume_mac_keyboard();
                    if let Err(err) = suspend_result {
                        tracing::warn!(
                            event = "tui_suspend_failed",
                            error = %err,
                            "failed to suspend TUI process"
                        );
                    }
                    return Some(TuiEvent::Resume);
                }
                Some(TuiEvent::Key(key_event))
            }
            Event::Resize(width, height) => {
                Some(TuiEvent::Resize(ratatui::layout::Size { width, height }))
            }
            Event::Paste(pasted) => Some(TuiEvent::Paste(pasted)),
            Event::FocusGained => {
                self.terminal_focused.store(true, Ordering::Relaxed);
                if let Some(focused) = &self.mac_keyboard_focused {
                    focused.store(true, Ordering::Relaxed);
                }
                // Keep the startup-cached palette: querying terminal colors here blocks the
                // input loop, and a direct probe would discard keys typed during the refresh.
                Some(TuiEvent::Draw)
            }
            Event::FocusLost => {
                self.terminal_focused.store(false, Ordering::Relaxed);
                if let Some(focused) = &self.mac_keyboard_focused {
                    focused.store(false, Ordering::Relaxed);
                }
                if let Some(pending) = &self.mac_keyboard_release_pending {
                    pending.store(false, Ordering::Relaxed);
                    Some(TuiEvent::Key(
                        super::mac_keyboard::MacRightOptionMonitor::release_event(),
                    ))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CmuxKeyEventDecision {
    Deliver,
    Drop,
    Release,
}

fn cmux_key_event_decision(
    focused: bool,
    previous_focus: Option<bool>,
    kind: KeyEventKind,
) -> CmuxKeyEventDecision {
    if focused {
        CmuxKeyEventDecision::Deliver
    } else if kind == KeyEventKind::Release && previous_focus == Some(true) {
        CmuxKeyEventDecision::Release
    } else {
        CmuxKeyEventDecision::Drop
    }
}

fn normalize_terminal_key_event(mut key_event: KeyEvent) -> KeyEvent {
    if key_event.modifiers == KeyModifiers::NONE
        && matches!(
            key_event.code,
            KeyCode::Char('\u{8}') | KeyCode::Char('\u{7f}')
        )
    {
        key_event.code = KeyCode::Backspace;
    }
    key_event
}

impl<S: EventSource + Default + Unpin> Unpin for TuiEventStream<S> {}

impl<S: EventSource + Default + Unpin> Stream for TuiEventStream<S> {
    type Item = TuiEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // approximate fairness + no starvation via round-robin.
        let draw_first = self.poll_draw_first;
        self.poll_draw_first = !self.poll_draw_first;

        if draw_first {
            if let Poll::Ready(event) = self.poll_draw_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_mac_keyboard_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_crossterm_event(cx) {
                return Poll::Ready(event);
            }
        } else {
            if let Poll::Ready(event) = self.poll_mac_keyboard_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_crossterm_event(cx) {
                return Poll::Ready(event);
            }
            if let Poll::Ready(event) = self.poll_draw_event(cx) {
                return Poll::Ready(event);
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;
    use std::task::Context;
    use std::task::Poll;
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tokio::sync::mpsc;
    use tokio::time::timeout;
    use tokio_stream::StreamExt;

    /// Simple fake event source for tests; feed events via the handle.
    struct FakeEventSource {
        rx: mpsc::UnboundedReceiver<EventResult>,
        tx: mpsc::UnboundedSender<EventResult>,
    }

    struct FakeEventSourceHandle {
        broker: Arc<EventBroker<FakeEventSource>>,
    }

    impl FakeEventSource {
        fn new() -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self { rx, tx }
        }
    }

    impl Default for FakeEventSource {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeEventSourceHandle {
        fn new(broker: Arc<EventBroker<FakeEventSource>>) -> Self {
            Self { broker }
        }

        fn send(&self, event: EventResult) {
            let mut state = self
                .broker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(source) = state.active_event_source_mut() else {
                return;
            };
            let _ = source.tx.send(event);
        }
    }

    impl EventSource for FakeEventSource {
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<EventResult>> {
            Pin::new(&mut self.get_mut().rx).poll_recv(cx)
        }
    }

    fn make_stream(
        broker: Arc<EventBroker<FakeEventSource>>,
        draw_rx: broadcast::Receiver<()>,
        terminal_focused: Arc<AtomicBool>,
    ) -> TuiEventStream<FakeEventSource> {
        TuiEventStream::new(
            broker,
            draw_rx,
            /*mac_keyboard_rx*/ None,
            /*mac_keyboard_focused*/ None,
            /*mac_keyboard_cmux_focus*/ None,
            /*mac_keyboard_paused*/ None,
            /*mac_keyboard_release_pending*/ None,
            terminal_focused,
            #[cfg(unix)]
            crate::tui::job_control::SuspendContext::new(),
            #[cfg(unix)]
            Arc::new(AtomicBool::new(false)),
        )
    }

    type SetupState = (
        Arc<EventBroker<FakeEventSource>>,
        FakeEventSourceHandle,
        broadcast::Sender<()>,
        broadcast::Receiver<()>,
        Arc<AtomicBool>,
    );

    fn setup() -> SetupState {
        let source = FakeEventSource::new();
        let broker = Arc::new(EventBroker::new());
        *broker.state.lock().unwrap() = EventBrokerState::Running(source);
        let handle = FakeEventSourceHandle::new(broker.clone());

        let (draw_tx, draw_rx) = broadcast::channel(1);
        let terminal_focused = Arc::new(AtomicBool::new(true));
        (broker, handle, draw_tx, draw_rx, terminal_focused)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn key_event_skips_unmapped() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        handle.send(Ok(Event::FocusLost));
        handle.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        ))));

        let next = stream.next().await.unwrap();
        match next {
            TuiEvent::Key(key) => {
                assert_eq!(key, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
            }
            other => panic!("expected key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_gained_preserves_already_queued_key() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        terminal_focused.store(false, Ordering::Relaxed);
        let mut stream = make_stream(broker.clone(), draw_rx, terminal_focused.clone());
        let expected_key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE);

        handle.send(Ok(Event::FocusGained));
        handle.send(Ok(Event::Key(expected_key)));

        assert!(matches!(stream.next().await, Some(TuiEvent::Draw)));
        assert!(terminal_focused.load(Ordering::Relaxed));
        assert!(matches!(
            &*broker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            EventBrokerState::Running(_)
        ));

        let next = timeout(Duration::from_millis(/*millis*/ 100), stream.next())
            .await
            .expect("focus handling discarded an already queued key");

        match next {
            Some(TuiEvent::Key(key)) => assert_eq!(key, expected_key),
            other => panic!("expected queued key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn draw_and_key_events_yield_both() {
        let (broker, handle, draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        let expected_key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let _ = draw_tx.send(());
        handle.send(Ok(Event::Key(expected_key)));

        let first = stream.next().await.unwrap();
        let second = stream.next().await.unwrap();

        let mut saw_draw = false;
        let mut saw_key = false;
        for event in [first, second] {
            match event {
                TuiEvent::Draw => {
                    saw_draw = true;
                }
                TuiEvent::Key(key) => {
                    assert_eq!(key, expected_key);
                    saw_key = true;
                }
                other => panic!("expected draw or key event, got {other:?}"),
            }
        }

        assert!(saw_draw && saw_key, "expected both draw and key events");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mac_keyboard_events_are_forwarded_as_key_events() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let (mac_keyboard_tx, mac_keyboard_rx) = broadcast::channel(1);
        let mac_keyboard_focused = Arc::new(AtomicBool::new(true));
        let mut stream = TuiEventStream::new(
            broker,
            draw_rx,
            Some(mac_keyboard_rx),
            Some(mac_keyboard_focused),
            /*mac_keyboard_cmux_focus*/ None,
            /*mac_keyboard_paused*/ None,
            /*mac_keyboard_release_pending*/ None,
            terminal_focused,
            #[cfg(unix)]
            crate::tui::job_control::SuspendContext::new(),
            #[cfg(unix)]
            Arc::new(AtomicBool::new(false)),
        );
        let expected_key = KeyEvent::new(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightAlt),
            KeyModifiers::NONE,
        );
        let _ = mac_keyboard_tx.send(expected_key);
        handle.send(Ok(Event::Resize(80, 24)));

        let next = timeout(Duration::from_millis(/*millis*/ 100), stream.next())
            .await
            .expect("timed out waiting for macOS keyboard event")
            .expect("macOS keyboard event stream ended");
        match next {
            TuiEvent::Key(key) => assert_eq!(key, expected_key),
            other => panic!("expected macOS keyboard event, got {other:?}"),
        }
    }

    #[test]
    fn cmux_push_to_talk_routes_to_only_the_focused_surface() {
        assert_eq!(
            cmux_key_event_decision(true, None, KeyEventKind::Press),
            CmuxKeyEventDecision::Deliver
        );
        assert_eq!(
            cmux_key_event_decision(false, None, KeyEventKind::Press),
            CmuxKeyEventDecision::Drop
        );
        assert_eq!(
            cmux_key_event_decision(false, Some(true), KeyEventKind::Release),
            CmuxKeyEventDecision::Release
        );
        assert_eq!(
            cmux_key_event_decision(false, Some(false), KeyEventKind::Release),
            CmuxKeyEventDecision::Drop
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_mac_keyboard_release_is_forwarded_before_input() {
        let (broker, _handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let (_mac_keyboard_tx, mac_keyboard_rx) = broadcast::channel(1);
        let mac_keyboard_focused = Arc::new(AtomicBool::new(true));
        let mac_keyboard_paused = Arc::new(AtomicBool::new(false));
        let mac_keyboard_release_pending = Arc::new(AtomicBool::new(true));
        let mut stream = TuiEventStream::new(
            broker,
            draw_rx,
            Some(mac_keyboard_rx),
            Some(mac_keyboard_focused),
            /*mac_keyboard_cmux_focus*/ None,
            Some(mac_keyboard_paused),
            Some(mac_keyboard_release_pending.clone()),
            terminal_focused,
            #[cfg(unix)]
            crate::tui::job_control::SuspendContext::new(),
            #[cfg(unix)]
            Arc::new(AtomicBool::new(false)),
        );

        let next = timeout(Duration::from_millis(/*millis*/ 100), stream.next())
            .await
            .expect("timed out waiting for pending macOS keyboard release")
            .expect("macOS keyboard event stream ended");
        match next {
            TuiEvent::Key(key) => {
                assert_eq!(key.kind, crossterm::event::KeyEventKind::Release);
                assert_eq!(
                    key.code,
                    KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightAlt)
                );
            }
            other => panic!("expected macOS keyboard release, got {other:?}"),
        }
        assert!(!mac_keyboard_release_pending.load(Ordering::Relaxed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_mac_keyboard_release_discards_queued_press() {
        let (broker, _handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let (mac_keyboard_tx, mac_keyboard_rx) = broadcast::channel(1);
        let mac_keyboard_focused = Arc::new(AtomicBool::new(true));
        let mac_keyboard_paused = Arc::new(AtomicBool::new(true));
        let mac_keyboard_release_pending = Arc::new(AtomicBool::new(true));
        let mut stream = TuiEventStream::new(
            broker,
            draw_rx,
            Some(mac_keyboard_rx),
            Some(mac_keyboard_focused),
            /*mac_keyboard_cmux_focus*/ None,
            Some(mac_keyboard_paused),
            Some(mac_keyboard_release_pending),
            terminal_focused,
            #[cfg(unix)]
            crate::tui::job_control::SuspendContext::new(),
            #[cfg(unix)]
            Arc::new(AtomicBool::new(false)),
        );
        let stale_press = KeyEvent::new(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightAlt),
            KeyModifiers::NONE,
        );
        let _ = mac_keyboard_tx.send(stale_press);

        let next = timeout(Duration::from_millis(/*millis*/ 100), stream.next())
            .await
            .expect("timed out waiting for pending macOS keyboard release")
            .expect("macOS keyboard event stream ended");
        assert!(matches!(
            next,
            TuiEvent::Key(KeyEvent {
                kind: crossterm::event::KeyEventKind::Release,
                ..
            })
        ));

        let fresh_press = KeyEvent::new(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightAlt),
            KeyModifiers::NONE,
        );
        let _ = mac_keyboard_tx.send(fresh_press);
        let next = timeout(Duration::from_millis(/*millis*/ 100), stream.next())
            .await
            .expect("timed out waiting for a fresh macOS keyboard press")
            .expect("macOS keyboard event stream ended");
        match next {
            TuiEvent::Key(key) => assert_eq!(key, fresh_press),
            other => panic!("expected a fresh macOS keyboard press, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_loss_forwards_mac_keyboard_release() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let (_mac_keyboard_tx, mac_keyboard_rx) = broadcast::channel(1);
        let mac_keyboard_focused = Arc::new(AtomicBool::new(true));
        let mac_keyboard_paused = Arc::new(AtomicBool::new(false));
        let mac_keyboard_release_pending = Arc::new(AtomicBool::new(false));
        let mut stream = TuiEventStream::new(
            broker,
            draw_rx,
            Some(mac_keyboard_rx),
            Some(mac_keyboard_focused),
            /*mac_keyboard_cmux_focus*/ None,
            Some(mac_keyboard_paused),
            Some(mac_keyboard_release_pending),
            terminal_focused.clone(),
            #[cfg(unix)]
            crate::tui::job_control::SuspendContext::new(),
            #[cfg(unix)]
            Arc::new(AtomicBool::new(false)),
        );
        handle.send(Ok(Event::FocusLost));

        let next = timeout(Duration::from_millis(/*millis*/ 100), stream.next())
            .await
            .expect("timed out waiting for focus-loss release")
            .expect("macOS keyboard event stream ended");
        match next {
            TuiEvent::Key(key) => {
                assert_eq!(key.kind, crossterm::event::KeyEventKind::Release);
                assert_eq!(
                    key.code,
                    KeyCode::Modifier(crossterm::event::ModifierKeyCode::RightAlt)
                );
            }
            other => panic!("expected focus-loss release, got {other:?}"),
        }
        assert!(!terminal_focused.load(Ordering::Relaxed));
    }

    #[test]
    fn terminal_key_rearms_mac_keyboard_after_focus_loss() {
        let (broker, _handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let (_mac_keyboard_tx, mac_keyboard_rx) = broadcast::channel(1);
        let mac_keyboard_focused = Arc::new(AtomicBool::new(false));
        let mut stream = TuiEventStream::new(
            broker,
            draw_rx,
            Some(mac_keyboard_rx),
            Some(mac_keyboard_focused.clone()),
            /*mac_keyboard_cmux_focus*/ None,
            /*mac_keyboard_paused*/ None,
            /*mac_keyboard_release_pending*/ None,
            terminal_focused,
            #[cfg(unix)]
            crate::tui::job_control::SuspendContext::new(),
            #[cfg(unix)]
            Arc::new(AtomicBool::new(false)),
        );

        let mapped = stream.map_crossterm_event(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        assert!(matches!(mapped, Some(TuiEvent::Key(_))));
        assert!(mac_keyboard_focused.load(Ordering::Relaxed));
    }

    #[test]
    fn csi_u_backspace_codepoints_map_to_backspace() {
        let (broker, _handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        for codepoint in ['\u{8}', '\u{7f}'] {
            let Some(TuiEvent::Key(key_event)) = stream.map_crossterm_event(Event::Key(
                KeyEvent::new(KeyCode::Char(codepoint), KeyModifiers::NONE),
            )) else {
                panic!("expected a key event");
            };
            assert_eq!(key_event.code, KeyCode::Backspace);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lagged_draw_maps_to_draw() {
        let (broker, _handle, draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx.resubscribe(), terminal_focused);

        // Fill channel to force Lagged on the receiver.
        let _ = draw_tx.send(());
        let _ = draw_tx.send(());

        let first = stream.next().await;
        assert!(matches!(first, Some(TuiEvent::Draw)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resize_event_maps_to_resize() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        handle.send(Ok(Event::Resize(80, 24)));

        let next = stream.next().await;
        assert!(matches!(
            next,
            Some(TuiEvent::Resize(ratatui::layout::Size {
                width: 80,
                height: 24
            }))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn error_or_eof_ends_stream() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        handle.send(Err(std::io::Error::other("boom")));

        let next = stream.next().await;
        assert!(next.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_wakes_paused_stream() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker.clone(), draw_rx, terminal_focused);

        broker.pause_events();

        let task = tokio::spawn(async move { stream.next().await });
        tokio::task::yield_now().await;

        broker.resume_events();
        let expected_key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        handle.send(Ok(Event::Key(expected_key)));

        let event = timeout(Duration::from_millis(100), task)
            .await
            .expect("timed out waiting for resumed event")
            .expect("join failed");
        match event {
            Some(TuiEvent::Key(key)) => assert_eq!(key, expected_key),
            other => panic!("expected key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_wakes_pending_stream() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker.clone(), draw_rx, terminal_focused);

        let task = tokio::spawn(async move { stream.next().await });
        tokio::task::yield_now().await;

        broker.pause_events();
        broker.resume_events();
        let expected_key = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        handle.send(Ok(Event::Key(expected_key)));

        let event = timeout(Duration::from_millis(100), task)
            .await
            .expect("timed out waiting for resumed event")
            .expect("join failed");
        match event {
            Some(TuiEvent::Key(key)) => assert_eq!(key, expected_key),
            other => panic!("expected key event, got {other:?}"),
        }
    }
}
