//! `lev dash` - Interactive terminal UI for managing concurrent agents.

mod graph;
mod helpers;
mod input;
mod mcp;
mod render;
mod state;
#[cfg(test)]
mod test_support;
mod theme;
mod types;

pub use helpers::yank_to_clipboard_via;
pub use types::{AgentDisplayStatus, DashboardAgent, DashboardArgs};

use crossterm::event::{Event, KeyEventKind};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use ratatui::Terminal;
use std::time::Duration;
use tokio::sync::mpsc;

use state::Dashboard;
use types::DaemonCommand;

/// Background task that forwards the dashboard's control commands (cancel /
/// answer-interaction / message) to the shared-world daemon over the control
/// socket. The dashboard is a pure client: it never drives agents itself.
async fn daemon_background_loop(
    control: ControlClient,
    mut cmd_rx: mpsc::UnboundedReceiver<DaemonCommand>,
    outcomes: mpsc::UnboundedSender<types::DaemonOutcome>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        let (run_id, request, what) = match cmd {
            DaemonCommand::Cancel { run_id } => {
                (run_id.clone(), ControlRequest::Cancel { run_id }, "cancel")
            }
            DaemonCommand::Answer { response } => (
                response.request_id.clone(),
                ControlRequest::AnswerInteraction { response },
                "answer",
            ),
            DaemonCommand::Message { agent_id, content } => (
                agent_id.clone(),
                ControlRequest::Message {
                    agent_id,
                    content,
                    target_region: None,
                },
                "message",
            ),
        };
        // Report what actually happened. This used to be discarded, so a cancel
        // the daemon refused was indistinguishable from one that worked.
        let outcome = match control.request(&request).await {
            Ok(ControlResponse::Ok { ok: true }) => types::DaemonOutcome {
                run_id,
                message: String::new(),
                ok: true,
            },
            Ok(ControlResponse::Ok { ok: false }) => types::DaemonOutcome {
                run_id,
                message: format!("the daemon has no such run to {what}"),
                ok: false,
            },
            Ok(other) => types::DaemonOutcome {
                run_id,
                message: format!("unexpected daemon response to {what}: {other:?}"),
                ok: false,
            },
            Err(e) => types::DaemonOutcome {
                run_id,
                message: format!("{what} failed: {e}"),
                ok: false,
            },
        };
        // A closed receiver means the dashboard has exited; nothing to report to.
        if outcomes.send(outcome).is_err() {
            return;
        }
    }
}

/// Abstracts "give me the next input event, or `None` if the poll timeout
/// elapses" (i.e. `crossterm::event::poll` + `event::read`), so the
/// dashboard's main loop (`run_dashboard_loop`) can be driven by canned
/// events in tests instead of blocking on a real terminal.
pub trait EventSource {
    fn poll_event(&mut self, timeout: Duration) -> std::io::Result<Option<Event>>;
}

/// Production [`EventSource`]: reads real terminal input via crossterm.
/// Uses injectable function pointers for `poll` and `read` so the two
/// branches of `poll_event` can be exercised in unit tests without a real
/// TTY.  In production, construct via [`CrosstermEventSource::new`]. Wired
/// into the real dashboard only by the binary's `real_dashboard`.
pub struct CrosstermEventSource {
    poll_fn: fn(Duration) -> std::io::Result<bool>,
    read_fn: fn() -> std::io::Result<Event>,
}

#[allow(clippy::new_without_default)] // constructed only by the binary's real_dashboard
impl CrosstermEventSource {
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

/// Abstracts terminal setup/teardown so `execute_core` can be tested with
/// a [`ratatui::backend::TestBackend`] and no-op TTY operations. The real
/// crossterm implementation (`CrosstermSetup`) lives in the binary — see
/// `real_dashboard` — since it can only be exercised against a real terminal.
pub trait TerminalSetup {
    type B: ratatui::backend::Backend;
    fn enable(&mut self) -> anyhow::Result<()>;
    fn create_terminal(&mut self) -> anyhow::Result<Terminal<Self::B>>;
    fn disable(&mut self);
    fn print_done(&self);
}

/// Terminal-independent core: runs the dashboard event loop after terminal
/// setup, driven in tests via [`TerminalSetup`] + [`EventSource`] without a real
/// TTY.
///
/// Generic over `S: TerminalSetup` and `E: EventSource`. The only
/// `TerminalSetup` in the library is the test double [`TestSetup`] (the real
/// `CrosstermSetup` lives in the binary), so every monomorphization here — and
/// in [`run_dashboard_loop`] — runs against a `ratatui::backend::TestBackend`
/// with canned events, keeping the whole function covered.
async fn execute_core<S: TerminalSetup, E: EventSource>(
    dashboard: &mut Dashboard,
    control: &ControlClient,
    setup: &mut S,
    events: &mut E,
) -> anyhow::Result<()> {
    setup.enable()?;
    let mut terminal = setup.create_terminal()?;
    let tick_rate = Duration::from_millis(100);
    run_dashboard_loop(dashboard, control, &mut terminal, events, tick_rate).await?;
    setup.disable();
    setup.print_done();
    Ok(())
}

/// The dashboard's per-tick render/input loop, extracted from [`execute`] so
/// it can run against a [`ratatui::backend::TestBackend`] and a canned
/// [`EventSource`] in tests, instead of a real terminal. Exits (returning
/// `Ok(())`) once `dashboard.should_quit` is set; propagates the first I/O
/// error from drawing or event polling, leaving raw mode / the alternate
/// screen untouched on error -- restoring those is `execute`'s
/// responsibility, not this loop's.
///
/// Generic over `B: Backend` and `impl EventSource`; in the measured test
/// build it is only ever instantiated once -- with the single
/// [`TestBackendHarness`] backend and the single [`TestEventSource`] (both
/// carry an injectable-failure switch, so the draw-error and poll-error `?`
/// arms are exercised within that one monomorphization), never a real
/// terminal backend.
async fn run_dashboard_loop<B: ratatui::backend::Backend>(
    dashboard: &mut Dashboard,
    control: &ControlClient,
    terminal: &mut Terminal<B>,
    events: &mut impl EventSource,
    tick_rate: Duration,
) -> anyhow::Result<()> {
    loop {
        dashboard.tick_count += 1;
        dashboard.tick_toasts();

        // Pull the daemon's open interactions (best-effort; ignore if the daemon
        // is unreachable) so waiting agents show their prompt.
        dashboard.sync_interactions(control).await;

        // …and which runs it actually holds, so a run on disk that nothing is
        // driving can be shown as stale rather than ACTIVE.
        dashboard.sync_daemon_runs(control).await;

        // Sync background runs from on-disk run-state dir (the daemon persists
        // meta/context/stages there).
        dashboard.sync_from_run_state();

        // Surface any completed MCP login/test as a toast.
        dashboard.drain_mcp_outcomes();

        // Report what the daemon did with this tick's commands.
        dashboard.drain_daemon_outcomes();

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

/// Builds the [`Dashboard`], starts the daemon-control background loop, and
/// seeds the startup log line. Split out of [`execute`] purely so this
/// (entirely terminal-independent) setup is unit-testable on its own, separate
/// from the real-terminal I/O sliver.
fn init_dashboard(control: ControlClient, yank_fn: fn(&str) -> bool) -> Dashboard {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // The real MCP screen operates on the user's config + token store, opens a
    // real browser for login, and reads the wall clock.
    let mcp_ctx = types::McpContext {
        config_path: crate::config::Config::config_path(),
        store_path: leviath_mcp::AuthStore::default_path().unwrap_or_default(),
        opener: std::sync::Arc::new(leviath_sys::open_url),
        clock: mcp_system_now,
    };
    let mut dashboard = Dashboard::new_with_log_path(
        cmd_tx,
        crate::runstate::dashboard_log_path(),
        yank_fn,
        mcp_ctx,
    );

    // Forward the dashboard's control commands to the daemon, and report each
    // result back so a refused command is surfaced rather than swallowed. A
    // freshly-built dashboard always has its outcome sender.
    let daemon_outcome_tx = dashboard
        .take_daemon_outcome_tx()
        .expect("a fresh dashboard has its daemon outcome sender");
    tokio::spawn(daemon_background_loop(control, cmd_rx, daemon_outcome_tx));

    // Run MCP logins/tests off the UI loop. A freshly-built dashboard always
    // has its background channel ends.
    let (mcp_cmd_rx, mcp_outcome_tx) = dashboard
        .take_mcp_bg_ends()
        .expect("a fresh dashboard has its MCP background channel ends");
    tokio::spawn(mcp::mcp_background_loop(
        dashboard.mcp_context(),
        mcp_cmd_rx,
        mcp_outcome_tx,
    ));

    dashboard.add_log("Dashboard started. Use `lev run <agent>` to start an agent.".to_string());

    dashboard
}

/// Wall-clock Unix time in seconds, for the production MCP context.
fn mcp_system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load config, build the dashboard + engine, and run the event loop against
/// the injected [`TerminalSetup`] and [`EventSource`]. This is the whole
/// `lev dash` command minus the two real-terminal doubles — so it is fully
/// unit-testable (drive it with `TestSetup` + a canned `TestEventSource`), and
/// the binary's `real_dashboard` supplies the real crossterm `CrosstermSetup`
/// + [`CrosstermEventSource`].
///
/// The real terminal wiring cannot live here: constructing `CrosstermSetup`
/// enables actual raw mode / the alternate screen and blocks forever on real
/// keyboard input (an `is_terminal()` guard does not prevent the hang -- it
/// still hangs a real editor terminal full-screen). That irreducible sliver is
/// the binary's job; everything it composes is exercised here.
/// `yank_fn` is the clipboard implementation the dashboard's `y` keypress uses;
/// the binary passes the real native-tool/OSC52 clipboard (which can write the
/// real terminal), tests pass a no-op.
pub async fn execute_with<S: TerminalSetup, E: EventSource>(
    control: ControlClient,
    setup: &mut S,
    events: &mut E,
    yank_fn: fn(&str) -> bool,
) -> anyhow::Result<()> {
    let mut dashboard = init_dashboard(control.clone(), yank_fn);
    execute_core(&mut dashboard, &control, setup, events).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::dashboard::test_support::make_test_dashboard;

    #[test]
    fn dashboard_args_can_be_constructed() {
        let _args = DashboardArgs {};
    }

    #[test]
    fn mcp_system_now_advances_past_the_epoch() {
        assert!(mcp_system_now() > 1_600_000_000);
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

    /// A control client pointing at a socket with no daemon behind it; requests
    /// fail fast, which the dashboard treats as "nothing to observe".
    fn no_daemon_control() -> ControlClient {
        let dir = std::env::temp_dir().join("leviath-dash-no-daemon");
        ControlClient::new(leviath_runtime::control_socket::control_id(&dir))
    }

    /// A fake daemon that accepts one connection, records the request line it
    /// receives, replies `{"result":"ok","ok":true}`, and returns the request.
    fn recording_daemon(dir: &std::path::Path) -> (ControlClient, tokio::task::JoinHandle<String>) {
        use leviath_runtime::control_socket::{bind_control_listener, control_id};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let req = lines.next_line().await.unwrap().unwrap_or_default();
            write_half
                .write_all(b"{\"result\":\"ok\",\"ok\":true}\n")
                .await
                .unwrap();
            req
        });
        (ControlClient::new(id), handle)
    }

    // ─── init_dashboard ──────────────────────────────────────────────────

    #[tokio::test]
    async fn init_dashboard_seeds_startup_log_and_forwards_commands() {
        crate::runstate::with_isolated_runs_dir_async(
            "init_dashboard_seeds_startup_log",
            |_d| async move {
                let dashboard = init_dashboard(no_daemon_control(), |_| false);
                assert!(
                    dashboard
                        .log
                        .iter()
                        .any(|entry| entry.message.contains("Dashboard started"))
                );
                // The background loop is live: a command on the dashboard's own
                // cmd_tx is accepted (delivered to the unreachable daemon).
                dashboard
                    .cmd_tx
                    .send(DaemonCommand::Cancel {
                        run_id: "nope".to_string(),
                    })
                    .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            },
        )
        .await;
    }

    // ─── daemon_background_loop ───────────────────────────────────────────

    #[tokio::test]
    async fn daemon_background_loop_forwards_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (control, server) = recording_daemon(dir.path());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        tokio::spawn(daemon_background_loop(control, cmd_rx, out_tx));
        cmd_tx
            .send(DaemonCommand::Cancel {
                run_id: "run-1".to_string(),
            })
            .unwrap();
        let req = server.await.unwrap();
        assert!(req.contains("cancel"));
        assert!(req.contains("run-1"));
    }

    #[tokio::test]
    async fn daemon_background_loop_forwards_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (control, server) = recording_daemon(dir.path());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        tokio::spawn(daemon_background_loop(control, cmd_rx, out_tx));
        cmd_tx
            .send(DaemonCommand::Answer {
                response: leviath_core::interaction::InteractionResponse::text("q1", "yes"),
            })
            .unwrap();
        let req = server.await.unwrap();
        assert!(req.contains("answer_interaction"));
        assert!(req.contains("q1"));
    }

    #[tokio::test]
    async fn daemon_background_loop_forwards_message() {
        let dir = tempfile::tempdir().unwrap();
        let (control, server) = recording_daemon(dir.path());
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        tokio::spawn(daemon_background_loop(control, cmd_rx, out_tx));
        cmd_tx
            .send(DaemonCommand::Message {
                agent_id: "a1".to_string(),
                content: "hi there".to_string(),
            })
            .unwrap();
        let req = server.await.unwrap();
        assert!(req.contains("message"));
        assert!(req.contains("hi there"));
    }

    #[tokio::test]
    async fn daemon_background_loop_exits_when_channel_dropped() {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(daemon_background_loop(no_daemon_control(), cmd_rx, out_tx));
        drop(cmd_tx);
        let result = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
        assert!(result.is_ok());
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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
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
            stage: "init".to_string(),
            stage_index: 0,
            num_stages: 1,
            status: AgentDisplayStatus::Idle,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
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
            taint_summary: vec![],
        };
        assert_eq!(agent.id, "run-test");
        assert_eq!(agent.blueprint_name, "tester");
        assert_eq!(agent.stage, "init");
    }

    // ─── run_dashboard_loop / EventSource ───────────────────────────────────

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    /// The single test [`EventSource`] used throughout this module. Keeping
    /// exactly one implementation means [`execute_core`] and
    /// [`run_dashboard_loop`] each monomorphize over just one concrete
    /// `EventSource` in the measured test build, so `cargo-llvm-cov`'s
    /// per-instantiation region report has no partially-covered sibling
    /// monomorphization to undercount.
    ///
    /// Two modes, both reachable from one type:
    /// - scripted: yields a fixed sequence (one `Option<Event>` per
    ///   `poll_event` call -- `Some(e)` -> `Ok(Some(e))`, `None` ->
    ///   `Ok(None)`, i.e. a simulated poll-timeout tick), then `None` forever
    ///   once exhausted.
    /// - failing (`fail = true`): every `poll_event` returns `Err`, to drive
    ///   `run_dashboard_loop`'s `?`-propagation path.
    struct TestEventSource {
        events: std::collections::VecDeque<Option<Event>>,
        fail: bool,
    }

    impl TestEventSource {
        /// Construct from a list of concrete events (all wrapped in `Some`).
        fn new(events: Vec<Event>) -> Self {
            Self {
                events: events.into_iter().map(Some).collect(),
                fail: false,
            }
        }

        /// Construct from a list of `Option<Event>`, allowing explicit `None`
        /// ticks (simulated poll timeouts with no input) to be interleaved.
        fn new_with_nones(events: Vec<Option<Event>>) -> Self {
            Self {
                events: events.into(),
                fail: false,
            }
        }

        /// Construct a source whose `poll_event` always errors.
        fn failing() -> Self {
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

    /// The single test [`ratatui::backend::Backend`] used throughout this
    /// module: a thin wrapper around a real [`ratatui::backend::TestBackend`]
    /// that adds a `fail_draw` switch. Using one backend type (rather than a
    /// separate always-failing backend) keeps [`run_dashboard_loop`] to a
    /// single monomorphization, so both the success and the `?`-error arms of
    /// its `terminal.draw(...)?` are exercised within the *same* instantiation
    /// -- leaving no partially-covered sibling for the region report to flag.
    struct TestBackendHarness {
        inner: ratatui::backend::TestBackend,
        fail_draw: bool,
    }

    impl TestBackendHarness {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: ratatui::backend::TestBackend::new(width, height),
                fail_draw: false,
            }
        }

        fn failing(width: u16, height: u16) -> Self {
            Self {
                inner: ratatui::backend::TestBackend::new(width, height),
                fail_draw: true,
            }
        }
    }

    impl ratatui::backend::Backend for TestBackendHarness {
        fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            if self.fail_draw {
                return Err(std::io::Error::other("simulated draw failure"));
            }
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> std::io::Result<()> {
            self.inner.hide_cursor()
        }
        fn show_cursor(&mut self) -> std::io::Result<()> {
            self.inner.show_cursor()
        }
        fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
            self.inner.get_cursor_position()
        }
        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::io::Result<()> {
            self.inner.set_cursor_position(position)
        }
        fn clear(&mut self) -> std::io::Result<()> {
            self.inner.clear()
        }
        fn size(&self) -> std::io::Result<ratatui::layout::Size> {
            self.inner.size()
        }
        fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
            self.inner.window_size()
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    fn test_terminal() -> Terminal<TestBackendHarness> {
        Terminal::new(TestBackendHarness::new(120, 40)).unwrap()
    }

    /// Test [`TerminalSetup`] backing [`execute_core`] with a
    /// [`TestBackendHarness`] terminal and no-op TTY operations, so the generic
    /// core (and the [`run_dashboard_loop`] it calls) monomorphizes only over
    /// test doubles in the measured test build -- never over the real
    /// `CrosstermBackend`, which can't be driven under `cargo test`. The two
    /// `_should_fail` flags let the error-propagation tests drive
    /// `execute_core`'s `setup.enable()?` and `setup.create_terminal()?`
    /// failure arms deterministically.
    struct TestSetup {
        enable_should_fail: bool,
        create_should_fail: bool,
    }

    impl TestSetup {
        fn new() -> Self {
            Self {
                enable_should_fail: false,
                create_should_fail: false,
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
            Terminal::new(TestBackendHarness::new(80, 24)).map_err(anyhow::Error::from)
        }

        fn disable(&mut self) {}

        fn print_done(&self) {}
    }

    #[tokio::test]
    async fn run_dashboard_loop_quits_on_esc_from_main_list() {
        let mut dashboard = make_test_dashboard();
        let control = no_daemon_control();
        let mut terminal = test_terminal();
        // A no-op Resize tick first, then the Esc that triggers quit --
        // covers both the `Event::Resize` and `Event::Key` match arms.
        let mut events = TestEventSource::new(vec![Event::Resize(80, 24), key(KeyCode::Esc)]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &control,
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
        let control = no_daemon_control();
        let mut terminal = test_terminal();
        // `None` entry → poll returns Ok(None) on tick 1 (no-event path);
        // `Some(Esc)` → poll returns Ok(Some(Esc)) on tick 2 → quit.
        let mut events = TestEventSource::new_with_nones(vec![None, Some(key(KeyCode::Esc))]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &control,
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
        let control = no_daemon_control();
        let mut terminal = test_terminal();
        let release = Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        ));
        let mut events = TestEventSource::new(vec![release, Event::FocusGained, key(KeyCode::Esc)]);

        let result = run_dashboard_loop(
            &mut dashboard,
            &control,
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
        let control = no_daemon_control();
        let mut terminal = test_terminal();
        let mut events = TestEventSource::failing();

        let result = run_dashboard_loop(
            &mut dashboard,
            &control,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_err());
    }

    // `crossterm::event::poll` cannot be called from a real unit test: it
    // hangs for 60+ seconds under a real pty, even in complete isolation
    // (`--test-threads=1`, nothing else running). Root cause:
    // `crossterm::event::poll`'s internal
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

    // ─── draw-error propagation ─────────────────────────────────────────────

    #[tokio::test]
    async fn run_dashboard_loop_propagates_draw_error() {
        // Exercises the `terminal.draw(…)?` error-propagation path using the
        // single `TestBackendHarness` backend with `fail_draw` set, so this
        // shares run_dashboard_loop's one monomorphization with the
        // success-path tests (the draw `?` has both arms covered there).
        let mut dashboard = make_test_dashboard();
        let control = no_daemon_control();
        let mut terminal = Terminal::new(TestBackendHarness::failing(120, 40)).unwrap();
        let mut events = TestEventSource::new(vec![]); // never reached

        let result = run_dashboard_loop(
            &mut dashboard,
            &control,
            &mut terminal,
            &mut events,
            Duration::from_millis(1),
        )
        .await;

        assert!(result.is_err());
    }

    // Caveat for anyone tempted to bound `run_dashboard_loop` with a
    // `tokio::time::timeout`: its `loop { ... }` has no `.await` point
    // (`poll_event`, `try_lock`, `terminal.draw` are all synchronous), so on
    // the default current-thread `#[tokio::test]` runtime the executor's
    // single thread never regains control long enough for the timeout's own
    // timer to fire -- a future that never yields can't be preempted by a
    // sibling future racing it. In a non-TTY sandbox
    // `CrosstermEventSource::poll_event` fails immediately (so it looks
    // bounded in headless testing); on a real terminal, with no scripted key
    // ever setting `should_quit`, it hangs indefinitely.

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

    // ─── CrosstermEventSource::poll_event: `?`-propagation branches ─────────
    //
    // Both `poll_event` tests above only ever exercise the `Ok` side of each
    // `?` (`(self.poll_fn)(timeout)?` and `(self.read_fn)()?`). Inject fake
    // fn pointers that return `Err` to exercise the error-propagation branch
    // of each `?` individually -- neither touches a real terminal.

    fn mock_poll_err(_: Duration) -> std::io::Result<bool> {
        Err(std::io::Error::other("simulated poll failure"))
    }

    fn mock_read_err() -> std::io::Result<Event> {
        Err(std::io::Error::other("simulated read failure"))
    }

    #[test]
    fn crossterm_event_source_poll_fn_error_propagates() {
        let mut src = CrosstermEventSource {
            poll_fn: mock_poll_err,
            read_fn: mock_read_esc,
        };
        let result = src.poll_event(Duration::from_millis(0));
        assert!(result.is_err());
    }

    #[test]
    fn crossterm_event_source_read_fn_error_propagates() {
        let mut src = CrosstermEventSource {
            poll_fn: mock_poll_true,
            read_fn: mock_read_err,
        };
        let result = src.poll_event(Duration::from_millis(0));
        assert!(result.is_err());
    }

    // ─── CrosstermEventSource::new constructor ──────────────────────────────
    //
    // The constructor only *stores* fn pointers (crossterm's real `poll`/`read`);
    // taking a function's address never invokes it, so constructing the type
    // touches no real terminal state.

    #[test]
    fn crossterm_event_source_new_constructs_without_touching_real_events() {
        let _src = CrosstermEventSource::new();
    }

    // ─── TestBackendHarness: delegated trait methods ─────────────────────────

    #[test]
    fn test_backend_harness_non_draw_methods_delegate() {
        // `terminal.draw(…)` in the loop tests only reaches a subset of the
        // backend's trait methods, so exercise the rest directly here. They
        // all just forward to the wrapped `TestBackend`; the `draw` success
        // arm is covered by the loop tests and the `fail_draw` arm by
        // `run_dashboard_loop_propagates_draw_error`.
        use ratatui::backend::Backend as _;
        let mut backend = TestBackendHarness::new(20, 10);
        assert!(backend.hide_cursor().is_ok());
        assert!(backend.show_cursor().is_ok());
        assert!(backend.get_cursor_position().is_ok());
        assert!(
            backend
                .set_cursor_position(ratatui::layout::Position::new(1, 1))
                .is_ok()
        );
        assert!(backend.clear().is_ok());
        assert!(backend.size().is_ok());
        assert!(backend.window_size().is_ok());
        assert!(backend.flush().is_ok());
    }

    // ─── execute_core / TestSetup ───────────────────────────────────────────
    //
    // `execute_core` and the `run_dashboard_loop` it calls are generic over
    // `TerminalSetup`/`EventSource`. The only `TerminalSetup` in the library is
    // the [`TestSetup`] double (the real `CrosstermSetup` lives in the binary),
    // so these tests drive every arm of `execute_core` against a `TestBackend`
    // deterministically, without touching a real terminal.

    #[tokio::test]
    async fn execute_core_happy_path_quits_on_esc() {
        crate::runstate::with_isolated_runs_dir_async(
            "execute_core_happy_path_quits_on_esc",
            |_d| async move {
                let control = no_daemon_control();
                let mut dashboard = init_dashboard(control.clone(), |_| false);
                let mut setup = TestSetup::new();
                let mut events = TestEventSource::new(vec![key(KeyCode::Esc)]);
                let result = execute_core(&mut dashboard, &control, &mut setup, &mut events).await;
                assert!(result.is_ok());
                assert!(dashboard.should_quit);
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_with_loads_config_inits_and_runs_the_loop() {
        // Drives the whole `execute_with` composition root (Config::load +
        // init_dashboard + execute_core) against the test terminal doubles,
        // with both the config path and the dashboard log path isolated so
        // nothing touches the developer's real ~/.leviath.
        crate::config::with_isolated_config_path_async(
            "execute_with_dashboard",
            |_fake_dir| async move {
                crate::runstate::with_isolated_runs_dir_async(
                    "execute_with_dashboard",
                    |_d| async move {
                        let mut setup = TestSetup::new();
                        let mut events = TestEventSource::new(vec![key(KeyCode::Esc)]);
                        let result =
                            execute_with(no_daemon_control(), &mut setup, &mut events, |_| false)
                                .await;
                        assert!(result.is_ok());
                    },
                )
                .await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_core_enable_error_propagates() {
        crate::runstate::with_isolated_runs_dir_async(
            "execute_core_enable_error_propagates",
            |_d| async move {
                let control = no_daemon_control();
                let mut dashboard = init_dashboard(control.clone(), |_| false);
                // `setup.enable()?` fails first, so the loop is never reached.
                let mut setup = TestSetup {
                    enable_should_fail: true,
                    create_should_fail: false,
                };
                let mut events = TestEventSource::new(vec![]);
                let result = execute_core(&mut dashboard, &control, &mut setup, &mut events).await;
                assert!(result.is_err());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_core_create_terminal_error_propagates() {
        crate::runstate::with_isolated_runs_dir_async(
            "execute_core_create_terminal_error_propagates",
            |_d| async move {
                let control = no_daemon_control();
                let mut dashboard = init_dashboard(control.clone(), |_| false);
                // `enable()` succeeds, then `create_terminal()?` fails -- deterministic
                // (no real backend / TTY involved), so this can never hang.
                let mut setup = TestSetup {
                    enable_should_fail: false,
                    create_should_fail: true,
                };
                let mut events = TestEventSource::new(vec![]);
                let result = execute_core(&mut dashboard, &control, &mut setup, &mut events).await;
                assert!(result.is_err());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execute_core_loop_error_propagates() {
        crate::runstate::with_isolated_runs_dir_async(
            "execute_core_loop_error_propagates",
            |_d| async move {
                let control = no_daemon_control();
                let mut dashboard = init_dashboard(control.clone(), |_| false);
                let mut setup = TestSetup::new();
                let mut events = TestEventSource::failing();
                let result = execute_core(&mut dashboard, &control, &mut setup, &mut events).await;
                assert!(result.is_err());
            },
        )
        .await;
    }

    // The real `lev dash` wiring (the crossterm `CrosstermSetup` + the real
    // `CrosstermEventSource` + the `Config::load`/`init_dashboard`/`execute_core`
    // composition) lives in the binary's `real_dashboard`; the fully-tested
    // seam it composes, `execute_with`, is covered by
    // `execute_with_loads_config_inits_and_runs_the_loop` above.
}
