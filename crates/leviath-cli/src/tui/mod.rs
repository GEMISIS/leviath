//! Seams shared by every Leviath terminal UI.
//!
//! A ratatui app has exactly two pieces that cannot run under `cargo test`:
//! taking over the real terminal (raw mode + alternate screen + a
//! `CrosstermBackend` on real stdout) and blocking on real keyboard input.
//! [`TerminalSetup`] and [`EventSource`] abstract those two, so a UI's whole
//! loop is unit-testable against a [`ratatui::backend::TestBackend`] and a
//! canned event list while the real crossterm bindings live in the
//! coverage-excluded `lev` binary.
//!
//! This started as `commands/dashboard`-private code. It moved here when the
//! `lev setup` wizard became a second ratatui surface: both drive the same
//! `CrosstermSetup` from `main.rs`, share [`theme`], and share the test doubles
//! below.
//!
//! ## Why the test doubles live here, not in each UI's test module
//!
//! `cargo-llvm-cov` reports generic functions per *instantiation*. A UI loop
//! generic over `B: Backend` that monomorphizes over two backend types gets two
//! region reports, and any arm exercised in only one of them shows as partially
//! covered. Keeping exactly one `TestEventSource` and one
//! `TestBackendHarness` for the whole crate means each loop monomorphizes
//! once, and both the success and the error arms of its `?`s land inside that
//! single instantiation. Both doubles therefore carry an injectable-failure
//! switch rather than having an always-failing sibling type.

pub(crate) mod keymap;
pub mod theme;
pub(crate) mod widgets;

use crossterm::event::Event;
use ratatui::Terminal;
use std::time::Duration;

/// Abstracts "give me the next input event, or `None` if the poll timeout
/// elapses" (i.e. `crossterm::event::poll` + `event::read`), so a UI's main
/// loop can be driven by canned events in tests instead of blocking on a real
/// terminal.
pub trait EventSource {
    /// Wait up to `timeout` for one input event. `Ok(None)` means the timeout
    /// elapsed with nothing to read, which is what lets a UI loop tick.
    fn poll_event(&mut self, timeout: Duration) -> std::io::Result<Option<Event>>;
}

/// Production [`EventSource`]: reads real terminal input via crossterm.
/// Uses injectable function pointers for `poll` and `read` so the two
/// branches of `poll_event` can be exercised in unit tests without a real
/// TTY.  In production, construct via [`CrosstermEventSource::new`]. Wired
/// into the real UIs only by the binary.
pub struct CrosstermEventSource {
    poll_fn: fn(Duration) -> std::io::Result<bool>,
    read_fn: fn() -> std::io::Result<Event>,
}

#[expect(
    clippy::new_without_default,
    reason = "constructed only by the binary's real UI entrypoints, where a Default would be a way to build one that reads no terminal"
)] // constructed only by the binary's real UI entrypoints
impl CrosstermEventSource {
    /// Read from the real terminal.
    pub fn new() -> Self {
        Self {
            poll_fn: crossterm::event::poll,
            read_fn: crossterm::event::read,
        }
    }
}

impl EventSource for CrosstermEventSource {
    fn poll_event(&mut self, timeout: Duration) -> std::io::Result<Option<Event>> {
        if (self.poll_fn)(timeout)? {
            Ok(Some((self.read_fn)()?))
        } else {
            Ok(None)
        }
    }
}

/// Abstracts terminal setup/teardown so a UI's generic core can be tested with
/// a [`ratatui::backend::TestBackend`] and no-op TTY operations. The real
/// crossterm implementation (`CrosstermSetup`) lives in the binary, since it
/// can only be exercised against a real terminal.
pub trait TerminalSetup {
    /// The backend the terminal draws through: crossterm in production, a
    /// `TestBackend` under test.
    type B: ratatui::backend::Backend;
    /// Take over the terminal - raw mode and the alternate screen.
    fn enable(&mut self) -> anyhow::Result<()>;
    /// Build the terminal the UI draws into. Called after [`Self::enable`].
    fn create_terminal(&mut self) -> anyhow::Result<Terminal<Self::B>>;
    /// Hand the terminal back. Must tolerate being called twice: the panic
    /// hook calls it too, and a panic during teardown would otherwise leave a
    /// terminal in raw mode.
    fn disable(&mut self);
    /// Print whatever should remain on screen after the UI exits.
    fn print_done(&self);
}

// ─── Test doubles (shared crate-wide; see the module docs for why) ───────────

#[cfg(test)]
pub(crate) use test_doubles::*;

#[cfg(test)]
mod test_doubles {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Build a plain unmodified key-press event, the overwhelmingly common
    /// shape in UI tests.
    pub(crate) fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    /// Build a key-press event carrying modifiers (`Ctrl-S`, `Shift-Tab`, …).
    pub(crate) fn key_with(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    /// The crate's single test [`EventSource`]. Two modes, both reachable from
    /// one type:
    /// - scripted: yields a fixed sequence (one `Option<Event>` per
    ///   `poll_event` call - `Some(e)` -> `Ok(Some(e))`, `None` -> `Ok(None)`,
    ///   i.e. a simulated poll-timeout tick), then `None` forever once
    ///   exhausted.
    /// - failing (`fail = true`): every `poll_event` returns `Err`, to drive a
    ///   loop's `?`-propagation path.
    pub(crate) struct TestEventSource {
        events: std::collections::VecDeque<Option<Event>>,
        fail: bool,
    }

    impl TestEventSource {
        /// Construct from a list of concrete events (all wrapped in `Some`).
        pub(crate) fn new(events: Vec<Event>) -> Self {
            Self {
                events: events.into_iter().map(Some).collect(),
                fail: false,
            }
        }

        /// Construct from a list of `Option<Event>`, allowing explicit `None`
        /// ticks (simulated poll timeouts with no input) to be interleaved.
        pub(crate) fn new_with_nones(events: Vec<Option<Event>>) -> Self {
            Self {
                events: events.into(),
                fail: false,
            }
        }

        /// Construct a source whose `poll_event` always errors.
        pub(crate) fn failing() -> Self {
            Self {
                events: std::collections::VecDeque::new(),
                fail: true,
            }
        }
    }

    impl EventSource for TestEventSource {
        fn poll_event(&mut self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            if self.fail {
                return Err(std::io::Error::other("simulated event source failure"));
            }
            Ok(self.events.pop_front().flatten())
        }
    }

    /// The crate's single test [`ratatui::backend::Backend`]: a thin wrapper
    /// around a real [`ratatui::backend::TestBackend`] that adds a `fail_draw`
    /// switch, so both the success and the `?`-error arms of a loop's
    /// `terminal.draw(...)?` are exercised within the *same* instantiation.
    pub(crate) struct TestBackendHarness {
        inner: ratatui::backend::TestBackend,
        fail_draw: bool,
    }

    impl TestBackendHarness {
        pub(crate) fn new(width: u16, height: u16) -> Self {
            Self {
                inner: ratatui::backend::TestBackend::new(width, height),
                fail_draw: false,
            }
        }

        pub(crate) fn failing(width: u16, height: u16) -> Self {
            Self {
                inner: ratatui::backend::TestBackend::new(width, height),
                fail_draw: true,
            }
        }

        /// The cells last drawn, so a test can assert on what a user would
        /// actually read rather than only that drawing did not panic.
        pub(crate) fn buffer(&self) -> &ratatui::buffer::Buffer {
            self.inner.buffer()
        }

        /// The drawn frame as newline-separated rows of text.
        pub(crate) fn text(&self) -> String {
            let buffer = self.buffer();
            let width = buffer.area.width as usize;
            buffer
                .content
                .chunks(width)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    /// ratatui 0.30's `TestBackend` is infallible (`Error = Infallible`);
    /// the harness keeps `io::Error` so the fail-draw switch still exercises
    /// the loops' error arms. `into_ok` converts the inner results: an
    /// `Infallible` error is a proof no error exists, so the conversion has
    /// no failure branch.
    fn into_ok<T>(result: Result<T, std::convert::Infallible>) -> std::io::Result<T> {
        match result {
            Ok(value) => Ok(value),
        }
    }

    impl ratatui::backend::Backend for TestBackendHarness {
        type Error = std::io::Error;

        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            if self.fail_draw {
                return Err(std::io::Error::other("simulated draw failure"));
            }
            into_ok(self.inner.draw(content))
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            into_ok(self.inner.hide_cursor())
        }
        fn show_cursor(&mut self) -> std::io::Result<()> {
            into_ok(self.inner.show_cursor())
        }
        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            into_ok(self.inner.get_cursor_position())
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::io::Result<()> {
            into_ok(self.inner.set_cursor_position(position))
        }
        fn clear(&mut self) -> std::io::Result<()> {
            into_ok(self.inner.clear())
        }
        fn clear_region(&mut self, region: ratatui::backend::ClearType) -> std::io::Result<()> {
            into_ok(self.inner.clear_region(region))
        }
        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            into_ok(self.inner.size())
        }
        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            into_ok(self.inner.window_size())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            into_ok(self.inner.flush())
        }
    }

    /// A ready-to-draw terminal over the shared test backend.
    pub(crate) fn test_terminal() -> Terminal<TestBackendHarness> {
        Terminal::new(TestBackendHarness::new(120, 40)).unwrap()
    }

    /// Test [`TerminalSetup`]: a [`TestBackendHarness`] terminal and no-op TTY
    /// operations, so a UI's generic core monomorphizes only over test doubles
    /// in the measured test build - never over the real `CrosstermBackend`,
    /// which can't be driven under `cargo test`. The two `_should_fail` flags
    /// drive the `setup.enable()?` and `setup.create_terminal()?` failure arms
    /// deterministically.
    pub(crate) struct TestSetup {
        pub(crate) enable_should_fail: bool,
        pub(crate) create_should_fail: bool,
        /// Hand back a backend whose every draw fails, so a loop's draw-error
        /// arm is reachable without a second `TerminalSetup` implementation.
        pub(crate) draw_should_fail: bool,
    }

    impl TestSetup {
        pub(crate) fn new() -> Self {
            Self {
                enable_should_fail: false,
                create_should_fail: false,
                draw_should_fail: false,
            }
        }
    }

    impl TerminalSetup for TestSetup {
        type B = TestBackendHarness;

        fn enable(&mut self) -> anyhow::Result<()> {
            if self.enable_should_fail {
                anyhow::bail!("simulated enable failure");
            }
            Ok(())
        }

        fn create_terminal(&mut self) -> anyhow::Result<Terminal<Self::B>> {
            if self.create_should_fail {
                anyhow::bail!("simulated create_terminal failure");
            }
            let backend = match self.draw_should_fail {
                true => TestBackendHarness::failing(80, 24),
                false => TestBackendHarness::new(80, 24),
            };
            Terminal::new(backend).map_err(anyhow::Error::from)
        }

        fn disable(&mut self) {}

        fn print_done(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    // ─── CrosstermEventSource ───────────────────────────────────────────────
    //
    // `poll_event` has four paths: poll-ready-then-read, poll-timeout, and the
    // `?` error arm of each call. Injecting fn pointers exercises all four with
    // no real TTY. The doubles are named fns reused across the tests rather
    // than per-test closures, because a closure passed only to a test that
    // never invokes it is itself an uncovered function.

    fn poll_ready(_: Duration) -> std::io::Result<bool> {
        Ok(true)
    }
    fn poll_timeout(_: Duration) -> std::io::Result<bool> {
        Ok(false)
    }
    fn poll_fails(_: Duration) -> std::io::Result<bool> {
        Err(std::io::Error::other("poll exploded"))
    }
    fn read_resize() -> std::io::Result<Event> {
        Ok(Event::Resize(80, 24))
    }
    fn read_fails() -> std::io::Result<Event> {
        Err(std::io::Error::other("read exploded"))
    }

    #[test]
    fn crossterm_event_source_returns_the_read_event_when_poll_reports_ready() {
        let mut source = CrosstermEventSource {
            poll_fn: poll_ready,
            read_fn: read_resize,
        };

        let event = source.poll_event(Duration::from_millis(1)).unwrap();

        assert_eq!(event, Some(Event::Resize(80, 24)));
    }

    #[test]
    fn crossterm_event_source_returns_none_when_poll_times_out() {
        // `read_fn` is supplied but must never run: a timeout tick reports no
        // event rather than reading one.
        let mut source = CrosstermEventSource {
            poll_fn: poll_timeout,
            read_fn: read_resize,
        };

        let event = source.poll_event(Duration::from_millis(1)).unwrap();

        assert!(event.is_none());
    }

    #[test]
    fn crossterm_event_source_propagates_a_poll_error() {
        let mut source = CrosstermEventSource {
            poll_fn: poll_fails,
            read_fn: read_resize,
        };

        let err = source.poll_event(Duration::from_millis(1)).unwrap_err();

        assert!(err.to_string().contains("poll exploded"));
    }

    #[test]
    fn crossterm_event_source_propagates_a_read_error() {
        let mut source = CrosstermEventSource {
            poll_fn: poll_ready,
            read_fn: read_fails,
        };

        let err = source.poll_event(Duration::from_millis(1)).unwrap_err();

        assert!(err.to_string().contains("read exploded"));
    }

    #[test]
    fn crossterm_event_source_new_stores_the_real_crossterm_functions() {
        // Taking a function's address never invokes it, so constructing the
        // production source touches no real terminal state.
        let _source = CrosstermEventSource::new();
    }

    #[test]
    fn test_event_source_yields_scripted_events_then_none_forever() {
        let mut source = TestEventSource::new(vec![key(KeyCode::Esc)]);

        assert_eq!(
            source.poll_event(Duration::from_millis(1)).unwrap(),
            Some(key(KeyCode::Esc))
        );
        // Exhausted: every later poll is a timeout tick, not an error.
        assert!(
            source
                .poll_event(Duration::from_millis(1))
                .unwrap()
                .is_none()
        );
        assert!(
            source
                .poll_event(Duration::from_millis(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn test_event_source_interleaves_explicit_timeout_ticks() {
        let mut source = TestEventSource::new_with_nones(vec![None, Some(key(KeyCode::Enter))]);

        assert!(
            source
                .poll_event(Duration::from_millis(1))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            source.poll_event(Duration::from_millis(1)).unwrap(),
            Some(key(KeyCode::Enter))
        );
    }

    #[test]
    fn test_event_source_failing_mode_errors_on_every_poll() {
        let mut source = TestEventSource::failing();

        assert!(source.poll_event(Duration::from_millis(1)).is_err());
        assert!(source.poll_event(Duration::from_millis(1)).is_err());
    }

    #[test]
    fn key_with_carries_its_modifiers() {
        let event = key_with(KeyCode::Char('s'), crossterm::event::KeyModifiers::CONTROL);

        assert_eq!(
            event,
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('s'),
                crossterm::event::KeyModifiers::CONTROL
            ))
        );
        // …and the plain helper does not.
        assert_ne!(event, key(KeyCode::Char('s')));
    }

    #[test]
    fn test_backend_harness_draws_or_fails_on_demand() {
        use ratatui::backend::Backend;

        let mut ok = TestBackendHarness::new(10, 3);
        assert!(ok.draw(std::iter::empty()).is_ok());
        // Every non-draw method delegates to the inner TestBackend.
        assert!(ok.hide_cursor().is_ok());
        assert!(ok.show_cursor().is_ok());
        assert!(ok.get_cursor_position().is_ok());
        assert!(
            ok.set_cursor_position(ratatui::layout::Position::new(0, 0))
                .is_ok()
        );
        assert!(ok.clear().is_ok());
        assert!(ok.clear_region(ratatui::backend::ClearType::All).is_ok());
        assert!(ok.size().is_ok());
        assert!(ok.window_size().is_ok());
        assert!(ok.flush().is_ok());

        let mut bad = TestBackendHarness::failing(10, 3);
        assert!(bad.draw(std::iter::empty()).is_err());
    }

    #[test]
    fn test_terminal_is_ready_to_draw() {
        let mut terminal = test_terminal();
        assert!(terminal.draw(|_| {}).is_ok());
    }

    #[test]
    fn test_setup_succeeds_by_default_and_fails_when_switched() {
        let mut setup = TestSetup::new();
        assert!(setup.enable().is_ok());
        assert!(setup.create_terminal().is_ok());
        setup.disable();
        setup.print_done();

        let mut enable_fails = TestSetup {
            enable_should_fail: true,
            create_should_fail: false,
            draw_should_fail: false,
        };
        assert!(enable_fails.enable().is_err());

        let mut create_fails = TestSetup {
            enable_should_fail: false,
            create_should_fail: true,
            draw_should_fail: false,
        };
        assert!(create_fails.create_terminal().is_err());
    }
}
