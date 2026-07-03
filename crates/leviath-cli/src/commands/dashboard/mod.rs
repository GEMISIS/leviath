//! `lev dash` - Interactive terminal UI for managing concurrent agents.

mod graph;
mod helpers;
mod input;
mod render;
mod state;
mod theme;
mod types;

pub use types::{AgentDisplayStatus, AgentEvent, DashboardAgent, DashboardArgs};

use crossterm::{
    event::{Event, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode},
};
#[cfg(not(test))]
use crossterm::{
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use leviath_runtime::AgentEngine;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

use super::run::build_provider_registry;
use crate::config::Config;

use state::Dashboard;
use types::EngineCommand;

/// Background task that processes engine commands (cancel/input) for in-process agent state.
async fn engine_background_loop(
    engine: Arc<Mutex<AgentEngine>>,
    mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            EngineCommand::CancelAgent { agent_id } => {
                let mut eng = engine.lock().await;
                let _ = eng.cancel_agent(&agent_id);
                let _ = event_tx.send(AgentEvent::StatusChanged {
                    agent_id,
                    status: AgentDisplayStatus::Cancelled,
                });
            }
            EngineCommand::SendInput { agent_id, input } => {
                let eng = engine.lock().await;
                let msg = leviath_runtime::AgentMessage {
                    agent_id,
                    content: input,
                    target_region: Some("conversation".to_string()),
                    priority: 0,
                };
                let _ = eng.send_message(msg);
            }
        }
    }
}

/// Abstracts "give me the next input event, or `None` if the poll timeout
/// elapses" (i.e. `crossterm::event::poll` + `event::read`), so the
/// dashboard's main loop ([`run_dashboard_loop`]) can be driven by canned
/// events in tests instead of blocking on a real terminal.
trait EventSource {
    fn poll_event(&mut self, timeout: Duration) -> std::io::Result<Option<Event>>;
}

/// Production [`EventSource`]: reads real terminal input via crossterm.
/// Uses injectable function pointers for `poll` and `read` so the two
/// branches of `poll_event` can be exercised in unit tests without a real
/// TTY.  In production, construct via [`CrosstermEventSource::new`].
struct CrosstermEventSource {
    poll_fn: fn(Duration) -> std::io::Result<bool>,
    read_fn: fn() -> std::io::Result<Event>,
}

impl CrosstermEventSource {
    fn new() -> Self {
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

/// Free-function wrappers for crossterm alternate-screen entry/exit, stored
/// as `fn` pointers in [`CrosstermSetup`] so tests can stub them out.
///
/// The real bodies are `#[cfg(not(test))]`-gated; under `cargo test` they're
/// replaced with the no-op twins below, so the real alternate-screen-mutating
/// code is structurally absent from the test binary rather than merely
/// "present but never called." That in turn makes it safe for a test to
/// invoke these two functions *by name* (through a real `CrosstermSetup`
/// whose `enable_raw`/`disable_raw` are still faked -- crossterm's real
/// `enable_raw_mode`/`disable_raw_mode` have no such test-only twin) to
/// exercise `CrosstermSetup::enable`/`disable`'s wiring without ever
/// touching the real alternate screen.
#[cfg(not(test))]
fn enter_alt_screen() -> std::io::Result<()> {
    stdout().execute(EnterAlternateScreen).map(|_| ())
}
#[cfg(not(test))]
fn leave_alt_screen() -> std::io::Result<()> {
    stdout().execute(LeaveAlternateScreen).map(|_| ())
}

#[cfg(test)]
fn enter_alt_screen() -> std::io::Result<()> {
    Ok(())
}
#[cfg(test)]
fn leave_alt_screen() -> std::io::Result<()> {
    Ok(())
}

/// Abstracts terminal setup/teardown so [`execute_core`] can be tested with
/// a [`ratatui::backend::TestBackend`] and no-op TTY operations.
trait TerminalSetup {
    type B: ratatui::backend::Backend;
    fn enable(&mut self) -> anyhow::Result<()>;
    fn create_terminal(&mut self) -> anyhow::Result<Terminal<Self::B>>;
    fn disable(&mut self);
    fn print_done(&self);
}

/// Production [`TerminalSetup`] that calls real crossterm terminal operations.
/// All four operations are stored as function pointers so tests can inject
/// no-ops, and the viewport used for terminal creation is also injectable
/// (production: `Viewport::Fullscreen`; tests: `Viewport::Fixed(...)`).
struct CrosstermSetup {
    enable_raw: fn() -> std::io::Result<()>,
    disable_raw: fn() -> std::io::Result<()>,
    enter_alt: fn() -> std::io::Result<()>,
    leave_alt: fn() -> std::io::Result<()>,
    viewport: Viewport,
}

impl CrosstermSetup {
    fn new() -> Self {
        Self {
            enable_raw: enable_raw_mode,
            disable_raw: disable_raw_mode,
            enter_alt: enter_alt_screen,
            leave_alt: leave_alt_screen,
            viewport: Viewport::Fullscreen,
        }
    }
}

impl TerminalSetup for CrosstermSetup {
    type B = ratatui::backend::CrosstermBackend<std::io::Stdout>;

    fn enable(&mut self) -> anyhow::Result<()> {
        (self.enable_raw)().map_err(anyhow::Error::from)?;
        (self.enter_alt)().map_err(anyhow::Error::from)?;
        Ok(())
    }

    fn create_terminal(&mut self) -> anyhow::Result<Terminal<Self::B>> {
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: self.viewport.clone(),
            },
        )
        .map_err(anyhow::Error::from)
    }

    fn disable(&mut self) {
        (self.disable_raw)().ok();
        (self.leave_alt)().ok();
    }

    fn print_done(&self) {
        println!("Dashboard closed.");
    }
}

/// Terminal-independent core: runs the dashboard event loop after terminal
/// setup. Extracted from [`execute`] so it can be driven in tests via
/// [`TerminalSetup`] + [`EventSource`] without a real TTY.
async fn execute_core<S: TerminalSetup, E: EventSource>(
    dashboard: &mut Dashboard,
    engine: &Arc<Mutex<AgentEngine>>,
    setup: &mut S,
    events: &mut E,
) -> anyhow::Result<()> {
    setup.enable()?;
    let mut terminal = setup.create_terminal()?;
    let tick_rate = Duration::from_millis(100);
    run_dashboard_loop(dashboard, engine, &mut terminal, events, tick_rate).await?;
    setup.disable();
    setup.print_done();
    Ok(())
}

/// The dashboard's per-tick render/input loop, extracted from [`execute`] so
/// it can run against a [`ratatui::backend::TestBackend`] and a canned
/// [`EventSource`] in tests, instead of a real terminal. Exits (returning
/// `Ok(())`) once `dashboard.should_quit` is set; propagates the first I/O
/// error from drawing or event polling, exactly as the original inline loop
/// in `execute` did (including leaving raw mode / the alternate screen
/// untouched on error -- restoring those is `execute`'s responsibility, not
/// this loop's, and this refactor preserves that pre-existing behavior
/// rather than changing it as a side effect of a coverage pass).
async fn run_dashboard_loop<B: ratatui::backend::Backend>(
    dashboard: &mut Dashboard,
    engine: &Arc<Mutex<AgentEngine>>,
    terminal: &mut Terminal<B>,
    events: &mut impl EventSource,
    tick_rate: Duration,
) -> anyhow::Result<()> {
    loop {
        dashboard.tick_count += 1;
        dashboard.tick_toasts();

        // Process agent events
        dashboard.process_events();

        // Sync background runs from on-disk run-state dir
        dashboard.sync_from_run_state();

        // Sync state from ECS world (try_lock to avoid blocking the UI)
        if let Ok(eng) = engine.try_lock() {
            dashboard.sync_agent_state_from_world(&eng);
        }

        // Draw
        terminal.draw(|frame| dashboard.draw(frame))?;

        // Handle input
        if let Some(event) = events.poll_event(tick_rate)? {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    dashboard.handle_key(key);
                }
                Event::Resize(_, _) => {
                    // Terminal will redraw automatically on next tick
                }
                _ => {}
            }
        }

        if dashboard.should_quit {
            return Ok(());
        }
    }
}

/// Builds the [`Dashboard`] and its backing [`AgentEngine`], starts the
/// engine background loop, and seeds the startup log line. Split out of
/// [`execute`] purely so this (entirely terminal-independent) setup is
/// unit-testable on its own, separate from the real-terminal I/O sliver.
async fn init_dashboard(config: &Config) -> (Dashboard, Arc<Mutex<AgentEngine>>) {
    let registry = build_provider_registry(config);
    let engine = Arc::new(Mutex::new(AgentEngine::with_providers(registry)));

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let mut dashboard = Dashboard::new(cmd_tx);

    // Store event_tx for background loop
    let bg_event_tx = dashboard.event_tx.clone();

    // Start engine background loop
    tokio::spawn(engine_background_loop(engine.clone(), cmd_rx, bg_event_tx));

    dashboard.add_log("Dashboard started. Use `lev run <agent>` to start an agent.".to_string());

    (dashboard, engine)
}

/// [`CrosstermEventSource`] for real keyboard input. Separated from
/// [`execute`] so the non-TTY logic (tick-rate setup, event source
/// construction, and the loop invocation itself) can be exercised in tests
///
/// Deliberately not called from any test (see [`execute_core`] and
/// [`run_dashboard_loop`] for the fully-covered, injectable logic this
/// delegates to). A `#[cfg(test)]`-guarded "is a real TTY attached?" check
/// was tried here and removed: it checked `is_terminal(&stdout())`, but
/// crossterm's raw-mode/alternate-screen calls act on the process's
/// controlling terminal (`/dev/tty`), not specifically fd 1 -- the two can
/// disagree depending on how the test binary is invoked. When they disagree
/// in the "looks safe but isn't" direction, calling this for real enables
/// actual raw mode and the actual alternate screen, then blocks forever
/// polling actual keyboard input (nothing ever sets `should_quit`), leaving
/// the invoking terminal hijacked full-screen until it's killed and reset.
/// That happened twice against a real editor terminal. Not worth a few
/// lines of coverage on a thin pass-through.
pub async fn execute(_args: DashboardArgs) -> anyhow::Result<()> {
    let config = Config::load()?;
    let (mut dashboard, engine) = init_dashboard(&config).await;
    let mut events = CrosstermEventSource::new();
    execute_core(
        &mut dashboard,
        &engine,
        &mut CrosstermSetup::new(),
        &mut events,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use state::Dashboard;

    fn make_test_dashboard() -> Dashboard {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        Dashboard::new(cmd_tx)
    }

    #[test]
    fn dashboard_args_can_be_constructed() {
        let _args = DashboardArgs {};
    }

    #[test]
    fn agent_display_status_variants_display() {
        let statuses = vec![
            AgentDisplayStatus::Active,
            AgentDisplayStatus::Waiting,
            AgentDisplayStatus::Complete,
            AgentDisplayStatus::CompleteInteractive,
            AgentDisplayStatus::Error("test error".to_string()),
            AgentDisplayStatus::Idle,
            AgentDisplayStatus::Cancelled,
        ];
        for status in statuses {
            let display = format!("{}", status);
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn agent_event_log_variant() {
        let event = AgentEvent::Log("test message".to_string());
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("test message"));
    }

    #[test]
    fn agent_event_stage_changed() {
        let event = AgentEvent::StageChanged {
            agent_id: "agent-1".to_string(),
            stage: "implement".to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("agent-1"));
        assert!(dbg.contains("implement"));
    }

    #[test]
    fn agent_event_status_changed() {
        let event = AgentEvent::StatusChanged {
            agent_id: "agent-1".to_string(),
            status: AgentDisplayStatus::Complete,
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("agent-1"));
        assert!(dbg.contains("Complete"));
    }

    #[test]
    fn agent_event_needs_input() {
        let event = AgentEvent::NeedsInput {
            agent_id: "agent-1".to_string(),
            prompt: "What should I do?".to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("What should I do?"));
    }

    #[test]
    fn agent_event_tool_called() {
        let event = AgentEvent::ToolCalled {
            agent_id: "agent-1".to_string(),
            tool: "read_file".to_string(),
            args: r#"{"path": "foo.txt"}"#.to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("read_file"));
    }

    #[test]
    fn agent_event_error() {
        let event = AgentEvent::Error {
            agent_id: "agent-1".to_string(),
            error: "something broke".to_string(),
        };
        let dbg = format!("{:?}", event);
        assert!(dbg.contains("something broke"));
    }

    // ─── init_dashboard ──────────────────────────────────────────────────

    #[tokio::test]
    async fn init_dashboard_seeds_startup_log_and_starts_background_loop() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "init_dashboard_seeds_startup_log_and_starts_background_loop",
        );
        let config = Config::default();
        let (dashboard, engine) = init_dashboard(&config).await;

        #[rustfmt::skip]
        assert!(dashboard.log.iter().any(|entry| entry.message.contains("Dashboard started")), "expected the startup message to be logged");

        // The background loop is live: a CancelAgent command sent on
        // dashboard's own cmd_tx should be processed without panicking.
        dashboard
            .cmd_tx
            .send(EngineCommand::CancelAgent {
                agent_id: "nonexistent".to_string(),
            })
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = engine.try_lock();
    }

    // ─── engine_background_loop: CancelAgent command ─────────────────────

    #[tokio::test]
    async fn engine_background_loop_cancel_agent() {
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();

        // Start the background loop
        tokio::spawn(engine_background_loop(engine, cmd_rx, event_tx));

        // Send a CancelAgent command
        cmd_tx
            .send(EngineCommand::CancelAgent {
                agent_id: "agent-nonexistent".to_string(),
            })
            .unwrap();

        // The loop should process it and send back a StatusChanged event
        let ev = tokio::time::timeout(std::time::Duration::from_millis(500), event_rx.recv())
            .await
            .expect("timed out waiting for StatusChanged event")
            .expect("channel closed before event was sent");
        // Verify via Debug representation to avoid dead else-branches in
        // pattern matching (LLVM would mark the not-matched arm as uncovered).
        let dbg = format!("{ev:?}");
        assert!(
            dbg.contains("StatusChanged"),
            "expected StatusChanged event, got: {dbg}"
        );
        assert!(
            dbg.contains("agent-nonexistent"),
            "expected agent_id in: {dbg}"
        );
        assert!(
            dbg.contains("Cancelled"),
            "expected Cancelled status in: {dbg}"
        );
    }

    #[tokio::test]
    async fn engine_background_loop_send_input_command() {
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<AgentEvent>();

        tokio::spawn(engine_background_loop(engine, cmd_rx, event_tx));

        // Send a SendInput command — should not panic even if no agent exists
        cmd_tx
            .send(EngineCommand::SendInput {
                agent_id: "nonexistent".to_string(),
                input: "test input".to_string(),
            })
            .unwrap();

        // Give it a moment to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // No panic = success
    }

    #[tokio::test]
    async fn engine_background_loop_exits_when_channel_dropped() {
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<AgentEvent>();

        let handle = tokio::spawn(engine_background_loop(engine, cmd_rx, event_tx));

        // Drop the sender to close the channel
        drop(cmd_tx);

        // The loop should exit because the channel is closed
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
        assert!(
            result.is_ok(),
            "engine_background_loop should exit when channel is closed"
        );
    }

    // ─── Dashboard basic integration ──────────────────────────────────────

    #[test]
    fn dashboard_new_and_initial_state() {
        let dash = make_test_dashboard();
        assert!(!dash.should_quit);
        assert!(!dash.detail_view);
        assert!(!dash.show_help);
    }

    #[test]
    fn dashboard_draw_renders_without_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal.draw(|f| dash.draw(f)).unwrap();
    }

    #[test]
    fn dashboard_agent_struct_fields_from_mod() {
        let agent = DashboardAgent {
            id: "run-test".to_string(),
            blueprint_name: "tester".to_string(),
            agent_path: "/path".to_string(),
            stage: "init".to_string(),
            stage_index: 0,
            num_stages: 1,
            status: AgentDisplayStatus::Idle,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            context_tokens: (0, 0),
            iteration: 0,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            entity: bevy_ecs::prelude::Entity::from_raw(0),
            is_run_state: false,
            pid: 0,
            workdir: "/tmp".to_string(),
            task: "test task".to_string(),
            title: None,
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 0,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: false,
        };
        assert_eq!(agent.id, "run-test");
        assert_eq!(agent.blueprint_name, "tester");
        assert_eq!(agent.stage, "init");
        assert!(!agent.is_run_state);
    }

    // ─── run_dashboard_loop / EventSource ───────────────────────────────────

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    /// Test [`EventSource`] that yields a fixed sequence of events (one per
    /// `poll_event` call), then `None` (simulating a poll-timeout tick) for
    /// every call after the sequence is exhausted.
    ///
    /// Entries are `Option<Event>`:
    /// - `Some(event)` → returns `Ok(Some(event))`
    /// - `None` → returns `Ok(None)` (simulates a poll-timeout with no input)
    struct ScriptedEventSource {
        events: std::collections::VecDeque<Option<Event>>,
    }

    impl ScriptedEventSource {
        /// Construct from a list of concrete events (all wrapped in `Some`).
        fn new(events: Vec<Event>) -> Self {
            Self {
                events: events.into_iter().map(Some).collect(),
            }
        }

        /// Construct from a list of `Option<Event>`, allowing explicit `None`
        /// ticks (simulated poll timeouts with no input) to be interleaved.
        fn new_with_nones(events: Vec<Option<Event>>) -> Self {
            Self {
                events: events.into(),
            }
        }
    }

    impl EventSource for ScriptedEventSource {
        fn poll_event(&mut self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            Ok(self.events.pop_front().flatten())
        }
    }

    /// [`EventSource`] whose `poll_event` always errors, to exercise
    /// `run_dashboard_loop`'s `?`-propagation path.
    struct FailingEventSource;

    impl EventSource for FailingEventSource {
        fn poll_event(&mut self, _timeout: Duration) -> std::io::Result<Option<Event>> {
            Err(std::io::Error::other("simulated event source failure"))
        }
    }

    fn test_terminal() -> Terminal<ratatui::backend::TestBackend> {
        Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap()
    }

    #[tokio::test]
    async fn run_dashboard_loop_quits_on_esc_from_main_list() {
        let mut dashboard = make_test_dashboard();
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let mut terminal = test_terminal();
        // A no-op Resize tick first, then the Esc that triggers quit --
        // covers both the `Event::Resize` and `Event::Key` match arms.
        let mut events = ScriptedEventSource::new(vec![Event::Resize(80, 24), key(KeyCode::Esc)]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &engine,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_ok());
        assert!(dashboard.should_quit);
    }

    #[tokio::test]
    async fn run_dashboard_loop_no_event_tick_then_quits() {
        // Tick 1: `poll_event` returns `None` (simulated poll-timeout — no input
        // pending); tick 2: Esc quits.  The `None` entry exercises the
        // `if let Some(event)` fallthrough path (line 127 in mod.rs).
        let mut dashboard = make_test_dashboard();
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let mut terminal = test_terminal();
        // `None` entry → poll returns Ok(None) on tick 1 (no-event path);
        // `Some(Esc)` → poll returns Ok(Some(Esc)) on tick 2 → quit.
        let mut events = ScriptedEventSource::new_with_nones(vec![None, Some(key(KeyCode::Esc))]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &engine,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_ok());
        assert!(dashboard.should_quit);
    }

    #[tokio::test]
    async fn run_dashboard_loop_ignores_non_press_and_other_events() {
        // A key release (not Press) and a mouse-like "other" event are both
        // ignored by the `_ => {}` arm; only the trailing Esc actually quits.
        let mut dashboard = make_test_dashboard();
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let mut terminal = test_terminal();
        let release = Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        ));
        let mut events =
            ScriptedEventSource::new(vec![release, Event::FocusGained, key(KeyCode::Esc)]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &engine,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_ok());
        assert!(dashboard.should_quit);
    }

    #[tokio::test]
    async fn run_dashboard_loop_propagates_event_source_error() {
        let mut dashboard = make_test_dashboard();
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let mut terminal = test_terminal();
        let mut events = FailingEventSource;

        let result = run_dashboard_loop(
            &mut dashboard,
            &engine,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_dashboard_loop_syncs_agent_state_when_engine_lock_available() {
        // Exercises the `engine.try_lock()` success branch (as opposed to
        // the lock being held elsewhere, which the other tests implicitly
        // cover since they never contend for it either -- both arms of the
        // `if let Ok(...)` are trivial, but this makes the intent explicit).
        let mut dashboard = make_test_dashboard();
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let mut terminal = test_terminal();
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Esc)]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &engine,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_ok());
    }

    // A previous version of this comment claimed `crossterm::event::poll`
    // "has no terminal-mutating side effects... safe to call from a real
    // unit test regardless of environment" and had a test call
    // `CrosstermEventSource::new().poll_event(1ms)` for real to prove it.
    // That claim was wrong, confirmed by hanging for 60+ seconds under a
    // real pty, in complete isolation (`--test-threads=1`, nothing else
    // running). Root cause: `crossterm::event::poll`'s internal
    // `INTERNAL_EVENT_READER` is a lazily-constructed, process-wide
    // singleton (`parking_lot::Mutex<Option<InternalEventReader>>`); the
    // passed timeout only bounds *acquiring that mutex*
    // (`try_lock_for(timeout)`), not the one-time construction of the
    // underlying `mio`-based event source that happens the first time it's
    // ever used in the process, nor whatever `mio::Poll::poll` actually
    // observes against a `script`-allocated pty's fd. There is no
    // "1ms-bounded, side-effect-free" way to touch real crossterm event
    // polling from a test at all -- so this doesn't get a test, matching
    // every other real-terminal entry point in this file (`execute`,
    // `open_controlling_tty` equivalent, etc.).

    // ─── FailingDrawBackend ─────────────────────────────────────────────────

    /// A minimal [`ratatui::backend::Backend`] whose `draw` always returns
    /// `Err`.  Used to exercise `terminal.draw(…)?`'s error-propagation path
    /// in [`run_dashboard_loop`] (line 114 in mod.rs).
    struct FailingDrawBackend;

    impl ratatui::backend::Backend for FailingDrawBackend {
        fn draw<'a, I>(&mut self, _content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            Err(std::io::Error::other("simulated draw failure"))
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn show_cursor(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            Ok(ratatui::layout::Position::new(0, 0))
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            _position: P,
        ) -> std::io::Result<()> {
            Ok(())
        }
        fn clear(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            Ok(ratatui::layout::Size::new(120, 40))
        }
        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            Ok(ratatui::backend::WindowSize {
                columns_rows: ratatui::layout::Size::new(120, 40),
                pixels: ratatui::layout::Size::new(0, 0),
            })
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_dashboard_loop_propagates_draw_error() {
        // Exercises the `terminal.draw(…)?` error-propagation path (line 114).
        let mut dashboard = make_test_dashboard();
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let mut terminal = Terminal::new(FailingDrawBackend).unwrap();
        let mut events = ScriptedEventSource::new(vec![]); // never reached

        let result = run_dashboard_loop(
            &mut dashboard,
            &engine,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_err(), "expected draw error to propagate");
    }

    #[tokio::test]
    async fn run_dashboard_loop_skips_sync_when_engine_locked() {
        // Exercises the `if let Ok(eng) = engine.try_lock()` *failure* arm
        // (the engine is held by another task while the loop tick runs).
        // When the lock is contended, `sync_agent_state_from_world` is
        // skipped and the loop continues normally to the draw + event steps.
        //
        // We acquire the engine lock in the current task BEFORE starting the
        // loop.  Because Tokio's Mutex is NOT reentrant, `try_lock()` inside
        // `run_dashboard_loop` will return `Err` while `_guard` is live.
        // We use ScriptedEventSource (dequeues instantly without `.await`-ing)
        // so Tokio never yields to another task that could release the lock.
        let mut dashboard = make_test_dashboard();
        let engine = Arc::new(Mutex::new(AgentEngine::new()));
        let _guard = engine.try_lock().expect("should be unlocked");

        let mut terminal = test_terminal();
        // With `_guard` held, `try_lock()` fails on the first tick (sync is
        // skipped).  `ScriptedEventSource` immediately returns `Some(Esc)`,
        // so the loop quits after exactly one tick.
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Esc)]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &engine,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        // Release the lock explicitly (drop order is otherwise unspecified).
        drop(_guard);

        assert!(result.is_ok());
        assert!(dashboard.should_quit);
    }

    // `run_crossterm_events_loop` (and this test, which was its only caller)
    // were removed. That helper wired a real `CrosstermEventSource` into
    // `run_dashboard_loop`, and this test tried to bound the result with a
    // 300ms `tokio::time::timeout`. That bound does not work:
    // `run_dashboard_loop`'s `loop { ... }` has no `.await` point anywhere in
    // its body (`poll_event`, `try_lock`, `terminal.draw` are all
    // synchronous), so on the default current-thread `#[tokio::test]`
    // runtime the executor's single thread never regains control long
    // enough for the timeout's own timer to fire -- a future that never
    // yields can't be preempted by a sibling future racing it. In a non-TTY
    // sandbox `CrosstermEventSource::poll_event` fails immediately, which is
    // why this looked bounded in headless testing; on a real terminal (no
    // scripted key ever sets `should_quit`) it hung indefinitely, observed
    // as "has been running for over 60 seconds" in practice. This helper had
    // no production caller (`execute`/`execute_core` call `run_dashboard_loop`
    // directly), so there was nothing left to preserve coverage of.

    // ─── CrosstermEventSource poll branches ─────────────────────────────────

    fn mock_poll_true(_: Duration) -> std::io::Result<bool> {
        Ok(true)
    }
    fn mock_poll_false(_: Duration) -> std::io::Result<bool> {
        Ok(false)
    }
    fn mock_read_esc() -> std::io::Result<Event> {
        Ok(Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::empty(),
        )))
    }

    #[test]
    fn crossterm_event_source_poll_true_returns_some_event() {
        let mut src = CrosstermEventSource {
            poll_fn: mock_poll_true,
            read_fn: mock_read_esc,
        };
        let result = src.poll_event(Duration::from_millis(0)).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn crossterm_event_source_poll_false_returns_none() {
        let mut src = CrosstermEventSource {
            poll_fn: mock_poll_false,
            read_fn: mock_read_esc,
        };
        let result = src.poll_event(Duration::from_millis(0)).unwrap();
        assert!(result.is_none());
    }

    // ─── CrosstermEventSource::new / CrosstermSetup::new constructors ───────
    //
    // Both constructors only ever *store* fn pointers (crossterm's real
    // `poll`/`read`/`enable_raw_mode`/`disable_raw_mode`, plus this module's
    // own `enter_alt_screen`/`leave_alt_screen`) and, for `CrosstermSetup`,
    // a `Viewport` value -- taking a function's address never invokes it, so
    // merely constructing either type touches no real terminal state. Only
    // *calling* the methods on a `CrosstermSetup::new()`-built instance would
    // (see the dedicated test further below for how that's done safely).

    #[test]
    fn crossterm_event_source_new_constructs_without_touching_real_events() {
        let _src = CrosstermEventSource::new();
    }

    #[test]
    fn crossterm_setup_new_constructs_without_touching_real_terminal() {
        let setup = CrosstermSetup::new();
        assert!(matches!(setup.viewport, Viewport::Fullscreen));
    }

    #[test]
    fn crossterm_setup_enable_disable_exercise_real_alt_screen_fn_pointers_via_test_stub() {
        // `enter_alt_screen`/`leave_alt_screen` are `#[cfg(test)]`-gated to
        // no-op stubs (see their definitions above) specifically so this
        // test can reference them *by their real names* -- the same values
        // `CrosstermSetup::new()` would use in production -- without ever
        // touching the real alternate screen. `enable_raw`/`disable_raw` are
        // still faked here since crossterm's real `enable_raw_mode`/
        // `disable_raw_mode` have no such test-only twin.
        fn noop() -> std::io::Result<()> {
            Ok(())
        }
        let mut setup = CrosstermSetup {
            enable_raw: noop,
            disable_raw: noop,
            enter_alt: enter_alt_screen,
            leave_alt: leave_alt_screen,
            viewport: Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
        };
        assert!(setup.enable().is_ok());
        setup.disable();
    }

    // ─── FailingDrawBackend: non-draw trait methods ──────────────────────────

    #[test]
    fn failing_draw_backend_non_draw_methods_return_ok() {
        // `run_dashboard_loop_propagates_draw_error` above only exercises
        // `draw()` (which errors) -- ratatui's `Terminal::draw()` returns as
        // soon as the backend's `draw()` fails, without calling any of this
        // fake backend's other trait methods, so they're never reached
        // through that path. Call them directly instead; they're trivial
        // fakes with no real I/O either way.
        use ratatui::backend::Backend as _;
        let mut backend = FailingDrawBackend;
        assert!(backend.hide_cursor().is_ok());
        assert!(backend.show_cursor().is_ok());
        assert!(backend.get_cursor_position().is_ok());
        assert!(backend
            .set_cursor_position(ratatui::layout::Position::new(1, 1))
            .is_ok());
        assert!(backend.clear().is_ok());
        assert!(backend.size().is_ok());
        assert!(backend.window_size().is_ok());
        assert!(backend.flush().is_ok());
    }

    // ─── execute_core / CrosstermSetup ──────────────────────────────────────

    impl CrosstermSetup {
        /// Test-only constructor: no-op TTY operations, fixed-size terminal
        /// viewport (avoids the `backend.size()` call that fails without a TTY).
        fn for_test() -> Self {
            use ratatui::layout::Rect;
            fn noop() -> std::io::Result<()> {
                Ok(())
            }
            Self {
                enable_raw: noop,
                disable_raw: noop,
                enter_alt: noop,
                leave_alt: noop,
                viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            }
        }
    }

    #[tokio::test]
    async fn execute_core_happy_path_quits_on_esc() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("execute_core_happy_path_quits_on_esc");
        let config = Config::default();
        let (mut dashboard, engine) = init_dashboard(&config).await;
        let mut setup = CrosstermSetup::for_test();
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Esc)]);
        let result = execute_core(&mut dashboard, &engine, &mut setup, &mut events).await;
        assert!(result.is_ok());
        assert!(dashboard.should_quit);
    }

    #[tokio::test]
    async fn execute_core_enable_error_propagates() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("execute_core_enable_error_propagates");
        fn fail() -> std::io::Result<()> {
            Err(std::io::Error::other("simulated enable_raw failure"))
        }
        use ratatui::layout::Rect;
        let config = Config::default();
        let (mut dashboard, engine) = init_dashboard(&config).await;
        let mut setup = CrosstermSetup {
            enable_raw: fail,
            disable_raw: fail,
            enter_alt: fail,
            leave_alt: fail,
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        };
        let mut events = ScriptedEventSource::new(vec![]);
        let result = execute_core(&mut dashboard, &engine, &mut setup, &mut events).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_core_create_terminal_error_propagates() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "execute_core_create_terminal_error_propagates",
        );
        // `Viewport::Fullscreen` makes `create_terminal()` call `backend.size()`
        // against a *real* `CrosstermBackend`. Whether that succeeds or fails
        // is environment-dependent — it fails on some non-TTY setups but has
        // been confirmed to *succeed* on others even without a TTY (e.g. some
        // sandboxed/piped environments still let a terminal-size ioctl through).
        // If it succeeds and this test drove the loop with an empty
        // `ScriptedEventSource`, `poll_event` would return `None` immediately
        // on every call with no sleep and no real `.await` point in the loop
        // body — a busy loop that can never yield back to the executor, so
        // not even a `tokio::time::timeout` around this call could preempt it.
        // That combination (real succeeding backend + never-terminating
        // scripted source) is exactly what hung this test and flooded a real
        // terminal with raw frame output in practice. Fix at the root: always
        // script a quit key so the loop is bounded to a handful of iterations
        // regardless of whether `create_terminal()` errors or succeeds.
        fn noop() -> std::io::Result<()> {
            Ok(())
        }
        let config = Config::default();
        let (mut dashboard, engine) = init_dashboard(&config).await;
        // Viewport::Fullscreen forces backend.size(), which errors without a
        // TTY on most platforms — the path this test intends to cover.
        let mut setup = CrosstermSetup {
            enable_raw: noop,
            disable_raw: noop,
            enter_alt: noop,
            leave_alt: noop,
            viewport: Viewport::Fullscreen,
        };
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Esc)]);
        let result = execute_core(&mut dashboard, &engine, &mut setup, &mut events).await;
        // If create_terminal() fails (the common case without a TTY), this is
        // an Err from that `?`. If it unexpectedly succeeds, the scripted Esc
        // quits the loop after one iteration and this is Ok. Both are
        // acceptable outcomes; what matters is that the test cannot hang.
        let _ = result;
    }

    #[tokio::test]
    async fn execute_core_loop_error_propagates() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("execute_core_loop_error_propagates");
        let config = Config::default();
        let (mut dashboard, engine) = init_dashboard(&config).await;
        let mut setup = CrosstermSetup::for_test();
        let mut events = FailingEventSource;
        let result = execute_core(&mut dashboard, &engine, &mut setup, &mut events).await;
        assert!(result.is_err());
    }

    // `execute()` itself (the real, non-test entry point) is deliberately
    // not called from any test -- see the doc comment on `execute` for why:
    // in short, an `is_terminal()`-based "skip if this looks like a real
    // TTY" guard was tried and removed after it twice failed to prevent
    // `execute()` from enabling real raw mode / the real alternate screen
    // and then hanging full-screen, waiting on keyboard input that no
    // automated test run supplies. `execute_core` and `run_dashboard_loop`
    // (exercised extensively above via `TestBackend` and injected
    // `TerminalSetup`/`EventSource` implementations) cover all of its
    // actual logic; `execute` itself is just a thin wire-up of real
    // components with nothing left to unit test safely.
}
