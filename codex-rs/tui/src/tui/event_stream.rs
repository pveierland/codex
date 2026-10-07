//! Event stream plumbing for the TUI.
//!
//! - [`EventBroker`] holds the shared crossterm stream so multiple callers reuse the same
//!   input source and can drop/recreate it on pause/resume without rebuilding consumers.
//! - [`TuiEventStream`] wraps a draw event subscription plus the shared [`EventBroker`] and maps crossterm
//!   events into [`TuiEvent`]. The broker also owns the tmux size monitor; its samples
//!   wake the draw subscription and become resize events before rendering.
//! - [`EventSource`] abstracts the underlying event producer; the real implementation is
//!   [`CrosstermEventSource`] and tests can swap in [`FakeEventSource`].
//! - Terminal theme changes arrive on the same input stream: a DEC mode 2031 notification or
//!   regained focus asks for the default colors without waiting, and the OSC 10/11 replies update
//!   the cached palette and repaint when the colors changed.
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
use std::time::Duration;

use crossterm::event::Event;
use crossterm::event::TerminalEvent;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::WatchStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use super::TuiEvent;
use super::size_monitor::SizeMonitor;

/// How long a due frame may wait while already-queued input is handled first.
///
/// Draining input before drawing makes a burst, such as a fast wheel flick, cost one frame
/// instead of one frame per event, so later input like a reversing notch acts at once. The bound
/// keeps the screen updating when input arrives faster than it can be handled.
const DRAW_DEFERRAL_LIMIT: Duration = Duration::from_millis(/*millis*/ 50);

/// Result type for terminal input events.
#[cfg(any(windows, test))]
pub type EventResult = std::io::Result<Event>;

/// Result type produced by an event source: input or a terminal color report.
pub type SourceResult = std::io::Result<TerminalEvent>;

/// Abstraction over a source of terminal events. Allows swapping in a fake for tests.
/// Value in production is [`CrosstermEventSource`].
pub trait EventSource: Send + 'static {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SourceResult>>;

    /// Ask the terminal for its default colors without waiting for the reply.
    ///
    /// Replies arrive later from [`Self::poll_next`] as [`TerminalEvent::Color`].
    fn query_default_colors(&mut self) {}
}

/// OSC 10/11 replies collected until both halves of one query have arrived.
#[derive(Debug, Default)]
struct DefaultColorReplies {
    fg: Option<(u8, u8, u8)>,
    bg: Option<(u8, u8, u8)>,
}

impl DefaultColorReplies {
    /// Record one reply and return the colors once foreground and background are both known.
    fn record(
        &mut self,
        slot: u8,
        color: Option<crossterm::style::Color>,
    ) -> Option<crate::terminal_probe::DefaultColors> {
        let rgb = match color {
            Some(crossterm::style::Color::Rgb { r, g, b }) => Some((r, g, b)),
            _ => None,
        };
        match slot {
            10 => self.fg = rgb,
            11 => self.bg = rgb,
            _ => return None,
        }
        let (Some(fg), Some(bg)) = (self.fg, self.bg) else {
            return None;
        };
        *self = Self::default();
        Some(crate::terminal_probe::DefaultColors { fg, bg })
    }
}

/// Shared crossterm input state for all [`TuiEventStream`] instances. A single crossterm EventStream
/// is reused so all streams still see the same input source.
///
/// This intermediate layer enables dropping/recreating the underlying EventStream (pause/resume) without rebuilding consumers.
pub struct EventBroker<S: EventSource = CrosstermEventSource> {
    state: Mutex<EventBrokerState<S>>,
    color_replies: Mutex<DefaultColorReplies>,
    resume_events_tx: watch::Sender<()>,
    pub(super) size_monitor: Option<SizeMonitor>,
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
            color_replies: Mutex::new(DefaultColorReplies::default()),
            resume_events_tx,
            size_monitor: None,
        }
    }

    /// Drop the underlying event source
    pub fn pause_events(&self) {
        if let Some(monitor) = &self.size_monitor {
            monitor.set_active(/*active*/ false);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = EventBrokerState::Paused;
    }

    /// Create a new instance of the underlying event source
    pub fn resume_events(&self) {
        if let Some(monitor) = &self.size_monitor {
            monitor.set_active(/*active*/ true);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = EventBrokerState::Start;
        let _ = self.resume_events_tx.send(());
    }

    /// Re-query default colors that the terminal reported before, in case its theme changed.
    ///
    /// Terminals that did not answer the startup probe are left alone, unless they announce a
    /// palette change themselves.
    pub fn refresh_default_colors(&self) {
        if crate::terminal_palette::default_colors().is_some() {
            self.query_default_colors();
        }
    }

    /// Ask the running event source for the terminal's default colors.
    fn query_default_colors(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(events) = state.active_event_source_mut() {
            events.query_default_colors();
        }
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
#[cfg_attr(not(windows), derive(Default))]
pub struct CrosstermEventSource {
    #[cfg(not(windows))]
    events: crossterm::event::TerminalEventStream,
    #[cfg(windows)]
    events: super::windows_key_sequence::WindowsKeySequence<crossterm::event::EventStream>,
}

#[cfg(windows)]
impl Default for CrosstermEventSource {
    fn default() -> Self {
        Self {
            events: super::windows_key_sequence::WindowsKeySequence::new(
                crossterm::event::EventStream::new(),
            ),
        }
    }
}

impl EventSource for CrosstermEventSource {
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SourceResult>> {
        // Crossterm's Windows backend expects Win32 input records. If VT input is inherited or
        // restored by another console client, navigation keys arrive as literal escape bytes.
        #[cfg(windows)]
        let _ = super::windows_console::ensure_input_record_mode();

        let result = Pin::new(&mut self.get_mut().events).poll_next(cx);

        // EventStream starts its blocking reader before returning Pending, so reassert the mode
        // after that transition as well.
        #[cfg(windows)]
        if result.is_pending() {
            let _ = super::windows_console::ensure_input_record_mode();
        }

        #[cfg(windows)]
        let result = result.map(|event| event.map(|event| event.map(TerminalEvent::Input)));

        result
    }

    #[cfg(not(windows))]
    fn query_default_colors(&mut self) {
        use std::io::Write;

        // Written between frames on the thread that draws, so it cannot split a frame's output.
        let mut stdout = std::io::stdout().lock();
        let result = stdout
            .write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\")
            .and_then(|()| stdout.flush());
        if let Err(err) = result {
            tracing::debug!("failed to query terminal default colors: {err}");
        }
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
    resume_stream: WatchStream<()>,
    terminal_focused: Arc<AtomicBool>,
    /// A due frame held back while queued input is handled, and when it became due.
    pending_draw: Option<(TuiEvent, Instant)>,
    #[cfg(unix)]
    suspend_context: crate::tui::job_control::SuspendContext,
    #[cfg(unix)]
    alt_screen_active: Arc<AtomicBool>,
}

impl<S: EventSource + Default + Unpin> TuiEventStream<S> {
    pub fn new(
        broker: Arc<EventBroker<S>>,
        draw_rx: broadcast::Receiver<()>,
        terminal_focused: Arc<AtomicBool>,
        #[cfg(unix)] suspend_context: crate::tui::job_control::SuspendContext,
        #[cfg(unix)] alt_screen_active: Arc<AtomicBool>,
    ) -> Self {
        let resume_stream = WatchStream::from_changes(broker.resume_events_rx());
        Self {
            broker,
            draw_stream: BroadcastStream::new(draw_rx),
            resume_stream,
            terminal_focused,
            pending_draw: None,
            #[cfg(unix)]
            suspend_context,
            #[cfg(unix)]
            alt_screen_active,
        }
    }

    /// Poll the shared crossterm stream for the next mapped `TuiEvent`.
    ///
    /// This skips events we don't use and keeps polling until it yields
    /// a mapped event, hits `Pending`, or sees EOF/error. When the broker is paused, it drops
    /// the underlying stream and returns `Pending` to fully release stdin.
    pub fn poll_crossterm_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        // Some crossterm events map to None (e.g. mouse); loop so we keep polling
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
                    Poll::Ready(Some(Ok(event))) => event,
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

            let event = match poll_result {
                TerminalEvent::Input(event) => event,
                TerminalEvent::ColorSchemeChanged => {
                    self.broker.query_default_colors();
                    continue;
                }
                TerminalEvent::Color { slot, color } => {
                    let colors = self
                        .broker
                        .color_replies
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .record(slot, color);
                    if colors.is_some_and(crate::terminal_palette::adopt_default_colors) {
                        return Poll::Ready(Some(TuiEvent::Draw));
                    }
                    continue;
                }
                TerminalEvent::OperatingStatus => continue,
            };
            if let Some(mapped) = self.map_crossterm_event(event) {
                return Poll::Ready(Some(mapped));
            }
        }
    }

    /// Poll the draw broadcast stream for the next draw event. Draw events are used to trigger a redraw of the TUI.
    pub fn poll_draw_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<TuiEvent>> {
        match Pin::new(&mut self.draw_stream).poll_next(cx) {
            Poll::Ready(Some(Ok(())))
            | Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {
                let event = self
                    .broker
                    .size_monitor
                    .as_ref()
                    .and_then(SizeMonitor::take_resize)
                    .map_or(TuiEvent::Draw, TuiEvent::Resize);
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    /// Map a crossterm event to a [`TuiEvent`], preserving mouse coordinates and modifiers.
    fn map_crossterm_event(&mut self, event: Event) -> Option<TuiEvent> {
        match event {
            Event::Key(key_event) => {
                #[cfg(unix)]
                if crate::tui::job_control::SUSPEND_KEY.is_press(key_event) {
                    self.broker.pause_events();
                    let suspend_result = self.suspend_context.suspend(&self.alt_screen_active);
                    self.broker.resume_events();
                    if let Err(err) = suspend_result {
                        tracing::warn!(
                            event = "tui_suspend_failed",
                            error = %err,
                            "failed to suspend TUI process"
                        );
                    }
                    self.broker.refresh_default_colors();
                    return Some(TuiEvent::Resume);
                }
                Some(TuiEvent::Key(key_event))
            }
            Event::Resize(width, height) => {
                let size = ratatui::layout::Size { width, height };
                if let Some(monitor) = &self.broker.size_monitor {
                    monitor.observe(size);
                }
                Some(TuiEvent::Resize(size))
            }
            Event::Paste(pasted) => Some(TuiEvent::Paste(pasted)),
            Event::FocusGained => {
                self.terminal_focused.store(true, Ordering::Relaxed);
                // The theme may have changed while another window had focus. The query does not
                // wait; its replies arrive through this stream like any other input.
                self.broker.refresh_default_colors();
                Some(TuiEvent::FocusGained)
            }
            Event::FocusLost => {
                self.terminal_focused.store(false, Ordering::Relaxed);
                Some(TuiEvent::FocusLost)
            }
            Event::Mouse(mouse) => Some(TuiEvent::Mouse(mouse)),
        }
    }
}

impl<S: EventSource + Default + Unpin> Unpin for TuiEventStream<S> {}

impl<S: EventSource + Default + Unpin> Stream for TuiEventStream<S> {
    type Item = TuiEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Input first: each queued event would otherwise wait for a full frame, so a burst of
        // wheel notches or keys would replay one frame at a time.
        if self.pending_draw.is_none() {
            match self.poll_draw_event(cx) {
                Poll::Ready(Some(event)) => self.pending_draw = Some((event, Instant::now())),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => {}
            }
        }
        if self
            .pending_draw
            .as_ref()
            .is_some_and(|(_, due)| due.elapsed() >= DRAW_DEFERRAL_LIMIT)
        {
            return Poll::Ready(self.pending_draw.take().map(|(event, _)| event));
        }
        if let Poll::Ready(event) = self.poll_crossterm_event(cx) {
            return Poll::Ready(event);
        }
        match self.pending_draw.take() {
            Some((event, _)) => Poll::Ready(Some(event)),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use crossterm::event::MouseEvent;
    use crossterm::event::MouseEventKind;
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
        rx: mpsc::UnboundedReceiver<SourceResult>,
        tx: mpsc::UnboundedSender<SourceResult>,
        color_queries: usize,
    }

    struct FakeEventSourceHandle {
        broker: Arc<EventBroker<FakeEventSource>>,
    }

    impl FakeEventSource {
        fn new() -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self {
                rx,
                tx,
                color_queries: 0,
            }
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
            self.send_terminal(event.map(TerminalEvent::Input));
        }

        fn send_terminal(&self, event: SourceResult) {
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

        fn color_queries(&self) -> usize {
            match &*self
                .broker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                EventBrokerState::Running(source) => source.color_queries,
                EventBrokerState::Paused | EventBrokerState::Start => 0,
            }
        }
    }

    impl EventSource for FakeEventSource {
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<SourceResult>> {
            Pin::new(&mut self.get_mut().rx).poll_recv(cx)
        }

        fn query_default_colors(&mut self) {
            self.color_queries += 1;
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
    async fn mouse_events_preserve_coordinates_and_do_not_consume_the_next_key() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let expected = MouseEvent {
            kind: MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 123,
            row: 42,
            modifiers: KeyModifiers::SHIFT,
        };
        handle.send(Ok(Event::Mouse(expected)));
        match stream.next().await {
            Some(TuiEvent::Mouse(actual)) => assert_eq!(actual, expected),
            other => panic!("expected mouse event, got {other:?}"),
        }
        let expected = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        handle.send(Ok(Event::Key(expected)));
        match stream.next().await {
            Some(TuiEvent::Key(actual)) => assert_eq!(actual, expected),
            other => panic!("expected key event, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_lost_is_forwarded_and_updates_terminal_state() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused.clone());

        handle.send(Ok(Event::FocusLost));

        assert!(matches!(stream.next().await, Some(TuiEvent::FocusLost)));
        assert!(!terminal_focused.load(Ordering::Relaxed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_lost_consumed_by_startup_remains_visible_to_the_tui() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut tui = crate::tui::test_support::make_test_tui().expect("test tui");
        tui.terminal_focused = terminal_focused.clone();
        let mut startup_events = make_stream(broker, draw_rx, terminal_focused);

        assert!(tui.is_terminal_focused());
        handle.send(Ok(Event::FocusLost));

        assert!(matches!(
            startup_events.next().await,
            Some(TuiEvent::FocusLost)
        ));
        assert!(!tui.is_terminal_focused());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_gained_preserves_already_queued_key() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        terminal_focused.store(false, Ordering::Relaxed);
        let mut stream = make_stream(broker.clone(), draw_rx, terminal_focused.clone());
        let expected_key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE);

        handle.send(Ok(Event::FocusGained));
        handle.send(Ok(Event::Key(expected_key)));

        assert!(matches!(stream.next().await, Some(TuiEvent::FocusGained)));
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn queued_input_is_handled_before_a_due_frame() {
        let (broker, handle, draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let wheel = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        let _ = draw_tx.send(());
        for _ in 0..3 {
            handle.send(Ok(Event::Mouse(wheel)));
        }

        for _ in 0..3 {
            assert!(matches!(stream.next().await, Some(TuiEvent::Mouse(_))));
        }
        assert!(matches!(stream.next().await, Some(TuiEvent::Draw)));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn sustained_input_cannot_hold_back_a_frame_indefinitely() {
        let (broker, handle, draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);

        let _ = draw_tx.send(());
        handle.send(Ok(Event::Key(key)));
        handle.send(Ok(Event::Key(key)));
        assert!(matches!(stream.next().await, Some(TuiEvent::Key(_))));

        tokio::time::advance(DRAW_DEFERRAL_LIMIT).await;
        assert!(matches!(stream.next().await, Some(TuiEvent::Draw)));
        assert!(matches!(stream.next().await, Some(TuiEvent::Key(_))));
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

    fn color(slot: u8, (r, g, b): (u8, u8, u8)) -> SourceResult {
        Ok(TerminalEvent::Color {
            slot,
            color: Some(crossterm::style::Color::Rgb { r, g, b }),
        })
    }

    /// Poll once with the given cached palette, as the next frame's input would see it.
    fn poll_with_palette(
        stream: &mut TuiEventStream<FakeEventSource>,
        palette: crate::terminal_probe::DefaultColors,
    ) -> Poll<Option<TuiEvent>> {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        crate::terminal_palette::with_test_default_colors(palette, || {
            stream.poll_crossterm_event(&mut cx)
        })
    }

    const DARK: crate::terminal_probe::DefaultColors = crate::terminal_probe::DefaultColors {
        fg: (220, 220, 220),
        bg: (20, 20, 20),
    };

    #[tokio::test(flavor = "current_thread")]
    async fn changed_default_colors_repaint_once_both_replies_arrive() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let expected_key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);

        handle.send_terminal(color(/*slot*/ 10, (20, 20, 20)));
        handle.send(Ok(Event::Key(expected_key)));
        assert!(matches!(
            poll_with_palette(&mut stream, DARK),
            Poll::Ready(Some(TuiEvent::Key(key))) if key == expected_key
        ));

        handle.send_terminal(color(/*slot*/ 11, (250, 250, 250)));
        assert!(matches!(
            poll_with_palette(&mut stream, DARK),
            Poll::Ready(Some(TuiEvent::Draw))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unchanged_default_colors_do_not_repaint() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        handle.send_terminal(color(/*slot*/ 10, DARK.fg));
        handle.send_terminal(color(/*slot*/ 11, DARK.bg));
        handle.send_terminal(Ok(TerminalEvent::OperatingStatus));

        assert!(poll_with_palette(&mut stream, DARK).is_pending());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn palette_notifications_query_colors_without_blocking_input() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);
        let expected_key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);

        handle.send_terminal(Ok(TerminalEvent::ColorSchemeChanged));
        handle.send(Ok(Event::Key(expected_key)));

        match stream.next().await {
            Some(TuiEvent::Key(key)) => assert_eq!(key, expected_key),
            other => panic!("expected key event, got {other:?}"),
        }
        assert_eq!(handle.color_queries(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn focus_gained_queries_colors_only_when_the_terminal_reports_them() {
        let (broker, handle, _draw_tx, draw_rx, terminal_focused) = setup();
        let mut stream = make_stream(broker, draw_rx, terminal_focused);

        handle.send(Ok(Event::FocusGained));
        assert!(matches!(stream.next().await, Some(TuiEvent::FocusGained)));
        assert_eq!(handle.color_queries(), 0);

        handle.send(Ok(Event::FocusGained));
        assert!(matches!(
            poll_with_palette(&mut stream, DARK),
            Poll::Ready(Some(TuiEvent::FocusGained))
        ));
        assert_eq!(handle.color_queries(), 1);
    }

    #[test]
    fn resumed_event_source_is_asked_for_colors_the_terminal_reported() {
        let (broker, handle, _draw_tx, _draw_rx, _terminal_focused) = setup();
        broker.pause_events();
        broker.resume_events();

        broker.refresh_default_colors();
        assert_eq!(handle.color_queries(), 0);

        crate::terminal_palette::with_test_default_colors(DARK, || {
            broker.refresh_default_colors();
        });
        assert_eq!(handle.color_queries(), 1);
    }

    #[test]
    fn unrecognized_color_reply_discards_its_half() {
        let mut replies = DefaultColorReplies::default();
        assert_eq!(
            replies.record(
                /*slot*/ 10,
                Some(crossterm::style::Color::Rgb { r: 1, g: 2, b: 3 })
            ),
            None
        );
        assert_eq!(replies.record(/*slot*/ 10, /*color*/ None), None);
        assert_eq!(
            replies.record(
                /*slot*/ 11,
                Some(crossterm::style::Color::Rgb { r: 4, g: 5, b: 6 })
            ),
            None
        );
        assert_eq!(
            replies.record(
                /*slot*/ 10,
                Some(crossterm::style::Color::Rgb { r: 7, g: 8, b: 9 })
            ),
            Some(crate::terminal_probe::DefaultColors {
                fg: (7, 8, 9),
                bg: (4, 5, 6),
            })
        );
    }
}
