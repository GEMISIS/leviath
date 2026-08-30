//! `lev dash` - Interactive terminal UI for managing concurrent agents.

mod agents;
mod click;
mod construct;
mod context_tree;
mod deny_feedback;
mod detail_band;
mod explorer;
mod graph;
mod helpers;
mod history;
mod input;
mod mcp;
mod new_run;
mod new_run_preview;
mod render;
mod run_actions;
mod selection;
mod state;
#[cfg(test)]
mod test_support;
mod types;
mod ui_state;

/// The palette lives in the crate-level [`crate::tui`] module, shared with the
/// `lev setup` wizard and the markdown renderer. Aliased here so the existing
/// `crate::commands::dashboard::theme::*` imports across `render/` keep
/// resolving unchanged.
use crate::tui::theme;

pub use helpers::yank_to_clipboard_via;
pub use types::DashboardArgs;
#[cfg(test)]
use types::{AgentDisplayStatus, DashboardAgent};

/// The terminal seams are crate-level too (`crate::tui`) now that `lev setup`
/// is a second ratatui surface driving the same `CrosstermSetup` from the
/// binary. Re-exported here because `main.rs` and the dashboard tests import
/// them through this path.
pub use crate::tui::{CrosstermEventSource, EventSource, TerminalSetup};

use crossterm::event::{Event, KeyEventKind};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use ratatui::Terminal;
use std::time::Duration;
use tokio::sync::mpsc;

use state::Dashboard;
use types::DaemonCommand;

/// How often the poller asks the daemon what it is holding.
///
/// Slower than the 100ms draw tick on purpose. Two socket round trips per
/// frame was never a rate anything needed - a run's status changes on the
/// order of seconds - and the old loop only ran at that rate because it was
/// doing the asking inline.
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Background task that asks the daemon what it is holding, and publishes each
/// round for the draw loop to pick up.
///
/// This exists so the dashboard never waits on the socket. The two questions -
/// the open interactions and the live run ids - used to be `await`ed inside the
/// tick, between advancing the frame and drawing it, so a daemon that was busy,
/// wedged, or part-way through a restart stopped the whole UI: no redraw, no
/// keys, nothing, until it answered. A control request's deadline is thirty
/// seconds and there were two per tick.
///
/// Every round is sent whether or not the daemon answered, because "it did not
/// say" is itself information the run list needs: a `None` run set means the
/// disk view is taken at face value rather than every run being marked stale.
///
/// The link bookkeeping this drives - reachable, restarted, build mismatch -
/// needs nothing sent: `ControlClient` shares it across clones, so the
/// dashboard's own handle sees what this task's requests learned.
async fn daemon_poll_loop(
    control: ControlClient,
    polls: mpsc::UnboundedSender<types::DaemonPoll>,
    interval: Duration,
) {
    loop {
        // A closed receiver means the dashboard has exited; nothing to poll for.
        if polls.send(poll_daemon(&control).await).is_err() {
            return;
        }
        tokio::time::sleep(interval).await;
    }
}

/// One round of the two questions, each answering `None` when the daemon did
/// not. Separate from the loop so a test can take a single round against a
/// socket instead of racing a timer.
async fn poll_daemon(control: &ControlClient) -> types::DaemonPoll {
    types::DaemonPoll {
        interactions: match control.request(&ControlRequest::ListInteractions).await {
            Ok(ControlResponse::Interactions { interactions }) => Some(interactions),
            _ => None,
        },
        run_ids: match control.request(&ControlRequest::List).await {
            Ok(ControlResponse::List { runs, .. }) => {
                Some(runs.into_iter().map(|entry| entry.run_id).collect())
            }
            _ => None,
        },
    }
}

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
            DaemonCommand::Pause { run_id } => {
                (run_id.clone(), ControlRequest::Pause { run_id }, "pause")
            }
            DaemonCommand::Resume { run_id } => {
                (run_id.clone(), ControlRequest::Resume { run_id }, "resume")
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
        // Report what actually happened. Discarding this would make a cancel
        // the daemon refused indistinguishable from one that worked.
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

/// Terminal-independent core: runs the dashboard event loop after terminal
/// setup, driven in tests via [`TerminalSetup`] + [`EventSource`] without a real
/// TTY.
///
/// Generic over `S: TerminalSetup` and `E: EventSource`. The only
/// `TerminalSetup` in the library is the test double [`TestSetup`] (the real
/// `CrosstermSetup` lives in the binary), so every monomorphization here - and
/// in [`run_dashboard_loop`] - runs against a `ratatui::backend::TestBackend`
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
    loop {
        run_dashboard_loop(dashboard, control, &mut terminal, events, tick_rate).await?;
        // The loop also returns when the agent editor wants `$EDITOR` on a
        // prompt: hand the terminal over, run it, take the terminal back and
        // carry on where the dashboard left off.
        let Some(edit) = dashboard.take_external_edit() else {
            break;
        };
        setup.disable();
        let ran = setup.run_editor(&edit.path);
        setup.enable()?;
        terminal = setup.create_terminal()?;
        dashboard.finish_external_edit(edit, ran);
    }
    setup.disable();
    setup.print_done();
    Ok(())
}

/// The dashboard's per-tick render/input loop, extracted from [`execute`] so
/// it can run against a [`ratatui::backend::TestBackend`] and a canned
/// [`EventSource`] in tests, instead of a real terminal. Exits (returning
/// `Ok(())`) once `dashboard.should_quit` is set; propagates the first I/O
/// error from drawing or event polling, leaving raw mode / the alternate
/// screen untouched on error - restoring those is `execute`'s
/// responsibility, not this loop's.
///
/// Generic over `B: Backend` and `impl EventSource`; in the measured test
/// build it is only ever instantiated once - with the single
/// [`TestBackendHarness`] backend and the single [`TestEventSource`] (both
/// carry an injectable-failure switch, so the draw-error and poll-error `?`
/// arms are exercised within that one monomorphization), never a real
/// terminal backend.
///
/// The draw error is mapped explicitly rather than propagated with a bare `?`.
/// On ratatui 0.29 `Backend::Error` is `io::Error` and either works; from 0.30
/// it becomes an associated type with no `Send + Sync` bound, and a bare `?`
/// into `anyhow::Error` stops compiling. Mapping here keeps this loop working
/// across that change without a `B::Error: Send + Sync + 'static` bound that
/// would have to be repeated on every caller and on `TerminalSetup::B`.
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

        // Take whatever the poller has learned: the daemon's open interactions,
        // so waiting agents show their prompt, and which runs it actually
        // holds, so a run on disk that nothing is driving shows as stale rather
        // than ACTIVE. Drained, never awaited - the socket must not be able to
        // stop the UI (see `daemon_poll_loop`).
        dashboard.drain_daemon_polls();

        // …and whether those polls reached a daemon at all, and which one.
        dashboard.sync_daemon_link(control);

        // …and whether the config file on disk still loads. One `stat` unless
        // it has been saved since the last tick.
        dashboard.sync_config_health();

        // Sync background runs from on-disk run-state dir (the daemon persists
        // meta/context/stages there).
        dashboard.sync_from_run_state();

        // Surface any completed MCP login/test as a toast.
        dashboard.drain_mcp_outcomes();

        // …and any run the new-run screen asked for.
        dashboard.drain_spawn_outcomes();
        dashboard.drain_agents_models();
        // A run started here opens its own page, once the daemon reports it.
        dashboard.open_pending_run();

        // Report what the daemon did with this tick's commands.
        dashboard.drain_daemon_outcomes();

        // Advance the stage graph's edge animation.
        dashboard.tick_graphs(tick_rate);

        // Draw
        terminal
            .draw(|frame| dashboard.draw(frame))
            .map_err(|e| anyhow::anyhow!("terminal draw failed: {e}"))?;

        // Handle input
        if let Some(event) = events.poll_event(tick_rate)? {
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    dashboard.handle_key(key);
                }
                // Wheel scrolling and click-drag text selection, handled in one
                // place (`selection.rs`) so they cannot disagree about state.
                Event::Mouse(m) => dashboard.handle_mouse(m),
                Event::Resize(_, _) => {
                    // Terminal will redraw automatically on next tick
                }
                _ => {}
            }
        }

        if dashboard.should_quit || dashboard.has_external_edit() {
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
        // Somebody is watching the MCP screen, so the handshake keeps the
        // deadline that is right for a person.
        connect_timeout: leviath_mcp::DEFAULT_CONNECT_TIMEOUT,
    };
    let mut dashboard = Dashboard::new_with_log_path(
        cmd_tx,
        crate::runstate::dashboard_log_path(),
        yank_fn,
        mcp_ctx,
        new_run::production_new_run_context(),
    );
    // The agent editor keeps its canvas arrangements under the data dir.
    dashboard.layout_store_path = crate::blueprint_edit::LayoutStore::default_path();
    // …and the run list keeps its folds beside them, restored before the first
    // sync so the list is drawn the way it was left rather than unfolding for a
    // frame first.
    dashboard.ui_state_path = crate::ui_state::default_path();
    dashboard.load_ui_state();

    // Forward the dashboard's control commands to the daemon, and report each
    // result back so a refused command is surfaced rather than swallowed. A
    // freshly-built dashboard always has its outcome sender.
    let daemon_outcome_tx = dashboard
        .take_daemon_outcome_tx()
        .expect("a fresh dashboard has its daemon outcome sender");
    tokio::spawn(daemon_background_loop(
        control.clone(),
        cmd_rx,
        daemon_outcome_tx,
    ));

    // Ask the daemon what it is holding, off the draw loop, for the same
    // reason: the UI keeps its own time whatever the socket is doing.
    let daemon_poll_tx = dashboard
        .take_daemon_poll_tx()
        .expect("a fresh dashboard has its daemon poll sender");
    tokio::spawn(daemon_poll_loop(
        control.clone(),
        daemon_poll_tx,
        DAEMON_POLL_INTERVAL,
    ));

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

    // Resolve-and-spawn for the new-run screen, off the UI loop for the same
    // reason. A freshly-built dashboard always has these ends too.
    let (spawn_cmd_rx, spawn_outcome_tx) = dashboard
        .take_spawn_bg_ends()
        .expect("a fresh dashboard has its spawn background channel ends");
    tokio::spawn(new_run::spawn_background_loop(
        control,
        spawn_cmd_rx,
        spawn_outcome_tx,
    ));

    dashboard.add_log("Dashboard started. Press `n` to start an agent run.".to_string());

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
/// `lev dash` command minus the two real-terminal doubles - so it is fully
/// unit-testable (drive it with `TestSetup` + a canned `TestEventSource`), and
/// the binary's `real_dashboard` supplies the real crossterm `CrosstermSetup`
/// + [`CrosstermEventSource`].
///
/// The real terminal wiring cannot live here: constructing `CrosstermSetup`
/// enables actual raw mode / the alternate screen and blocks forever on real
/// keyboard input (an `is_terminal()` guard does not prevent the hang - it
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
    // The render loop's own polls must fail fast: they run inline at ten a
    // second, and a poll that waited a restart out would freeze the screen for
    // as long as the daemon was down. The background loops keep the patient
    // client, so a cancel or a spawn asked for mid-restart lands once the
    // daemon is back instead of failing.
    let poll = control.with_reconnect_grace(std::time::Duration::ZERO);
    execute_core(&mut dashboard, &poll, setup, events).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::commands::dashboard::test_support::make_test_dashboard;

    #[test]
    fn dashboard_args_can_be_constructed() {
        let _args = DashboardArgs {};
    }

    /// The poller publishes a round whether or not the daemon answered, and
    /// stops once nobody is listening.
    ///
    /// A round that answered nothing is not a round to skip: `None` run ids
    /// mean "the daemon did not say", which is what stops the run list
    /// condemning every run on disk as stale.
    #[tokio::test]
    async fn the_poll_loop_publishes_rounds_until_the_dashboard_goes() {
        use leviath_runtime::control_socket::control_id;
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let control = ControlClient::new(control_id(&dir.path().join("no-daemon")));
        let task = tokio::spawn(daemon_poll_loop(
            control,
            tx,
            std::time::Duration::from_millis(1),
        ));

        let round = rx.recv().await.expect("a round is published");
        assert_eq!(
            round,
            types::DaemonPoll::default(),
            "nothing answered, and the round says so rather than not arriving"
        );

        // Dropping the receiver is how the dashboard says it has exited.
        drop(rx);
        task.await.expect("the loop returns rather than hanging");
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
        let dir =
            std::env::temp_dir().join(format!("leviath-dash-no-daemon-{}", std::process::id()));
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
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
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

    /// A daemon that replies with `reply` (verbatim, newline added) to one
    /// request. `None` closes the connection without replying.
    fn replying_daemon(
        dir: &std::path::Path,
        reply: Option<&'static str>,
    ) -> (ControlClient, tokio::task::JoinHandle<()>) {
        use leviath_runtime::control_socket::{bind_control_listener, control_id};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let id = control_id(dir);
        let mut listener = bind_control_listener(&id).unwrap();
        let handle = tokio::spawn(async move {
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut lines = BufReader::new(read_half).lines();
            let _ = lines.next_line().await;
            if let Some(reply) = reply {
                let _ = write_half.write_all(format!("{reply}\n").as_bytes()).await;
            }
        });
        (ControlClient::new(id), handle)
    }

    /// Drive one cancel through the loop and return the reported outcome.
    async fn cancel_outcome(reply: Option<&'static str>) -> types::DaemonOutcome {
        let dir = tempfile::tempdir().unwrap();
        let (control, server) = replying_daemon(dir.path(), reply);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        tokio::spawn(daemon_background_loop(control, cmd_rx, out_tx));
        cmd_tx
            .send(DaemonCommand::Cancel {
                run_id: "run-1".to_string(),
            })
            .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await
            .expect("an outcome was reported")
            .expect("the loop is alive");
        let _ = server.await;
        outcome
    }

    /// Every response shape the daemon can give is reported back, so the
    /// dashboard can tell a kill that worked from one that did not.
    #[tokio::test]
    async fn daemon_background_loop_reports_each_outcome() {
        let ok = cancel_outcome(Some(r#"{"result":"ok","ok":true}"#)).await;
        assert!(ok.ok, "an applied cancel is reported as success");
        assert_eq!(ok.run_id, "run-1");

        let missing = cancel_outcome(Some(r#"{"result":"ok","ok":false}"#)).await;
        assert!(!missing.ok);
        assert!(missing.message.contains("no such run to cancel"));

        let odd = cancel_outcome(Some(r#"{"result":"spawned","run_id":"x"}"#)).await;
        assert!(!odd.ok);
        assert!(odd.message.contains("unexpected daemon response"));

        // Connection closed with no reply → a transport error, surfaced as such.
        let broken = cancel_outcome(None).await;
        assert!(!broken.ok);
        assert!(
            broken.message.contains("cancel failed"),
            "got: {}",
            broken.message
        );
    }

    /// Drive one arbitrary command through the loop and return the outcome.
    async fn command_outcome(
        cmd: DaemonCommand,
        reply: Option<&'static str>,
    ) -> types::DaemonOutcome {
        let dir = tempfile::tempdir().unwrap();
        let (control, server) = replying_daemon(dir.path(), reply);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        tokio::spawn(daemon_background_loop(control, cmd_rx, out_tx));
        cmd_tx.send(cmd).unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await
            .expect("an outcome was reported")
            .expect("the loop is alive");
        let _ = server.await;
        outcome
    }

    /// Pause and resume ride the same forwarding path as cancel; the daemon's
    /// refusal is labelled with the verb the user actually pressed.
    #[tokio::test]
    async fn daemon_background_loop_forwards_pause_and_resume() {
        let ok = command_outcome(
            DaemonCommand::Pause {
                run_id: "run-1".to_string(),
            },
            Some(r#"{"result":"ok","ok":true}"#),
        )
        .await;
        assert!(ok.ok);
        assert_eq!(ok.run_id, "run-1");

        let refused = command_outcome(
            DaemonCommand::Pause {
                run_id: "run-1".to_string(),
            },
            Some(r#"{"result":"ok","ok":false}"#),
        )
        .await;
        assert!(!refused.ok);
        assert!(refused.message.contains("no such run to pause"));

        let ok = command_outcome(
            DaemonCommand::Resume {
                run_id: "run-1".to_string(),
            },
            Some(r#"{"result":"ok","ok":true}"#),
        )
        .await;
        assert!(ok.ok);

        let refused = command_outcome(
            DaemonCommand::Resume {
                run_id: "run-1".to_string(),
            },
            Some(r#"{"result":"ok","ok":false}"#),
        )
        .await;
        assert!(!refused.ok);
        assert!(refused.message.contains("no such run to resume"));
    }

    /// The loop stops when the dashboard has gone away, rather than spinning on
    /// a channel nobody reads.
    #[tokio::test]
    async fn daemon_background_loop_exits_when_the_dashboard_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (control, _server) = replying_daemon(dir.path(), Some(r#"{"result":"ok","ok":true}"#));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        drop(out_rx); // the dashboard exited
        let handle = tokio::spawn(daemon_background_loop(control, cmd_rx, out_tx));
        cmd_tx
            .send(DaemonCommand::Cancel {
                run_id: "run-1".to_string(),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("the loop returned")
            .unwrap();
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
        // An empty dashboard still draws its chrome. Without this the test
        // would pass just as happily against a `draw` that returned early.
        let buf = crate::commands::dashboard::test_support::rendered_buffer(&terminal);
        assert!(buf.contains("Agent Runs"), "{buf}");
    }

    #[test]
    fn dashboard_agent_struct_fields_from_mod() {
        let agent = DashboardAgent {
            id: "run-test".to_string(),
            blueprint_name: "tester".to_string(),
            stage: "init".to_string(),
            stage_index: 0,
            num_stages: 1,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp".to_string(),
            task: "test task".to_string(),
            title: None,
            model: None,
            parent_id: None,
            started_at: 0,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: false,
        };
        assert_eq!(agent.id, "run-test");
        assert_eq!(agent.blueprint_name, "tester");
        assert_eq!(agent.stage, "init");
    }

    // ─── run_dashboard_loop ─────────────────────────────────────────────────
    //
    // The terminal doubles these tests drive (`TestEventSource`,
    // `TestBackendHarness`, `TestSetup`, `key`) are crate-level, in
    // [`crate::tui`], and shared with the `lev setup` wizard's tests. Keeping
    // exactly one implementation of each is load-bearing for coverage: it means
    // [`execute_core`] and [`run_dashboard_loop`] monomorphize over a single
    // concrete backend / event source in the measured test build, so
    // `cargo-llvm-cov`'s per-instantiation region report has no partially
    // covered sibling monomorphization to undercount.

    use crate::tui::{TestBackendHarness, TestEventSource, TestSetup, key, test_terminal};
    use crossterm::event::KeyCode;

    #[tokio::test]
    async fn run_dashboard_loop_quits_on_q_from_main_list() {
        let mut dashboard = make_test_dashboard();
        let control = no_daemon_control();
        let mut terminal = test_terminal();
        // A no-op Resize tick, then both wheel directions, a full
        // press-drag-release selection over the log panel, and the q that
        // triggers quit - covers every arm of the event match, including the
        // mouse one that carries scrolling and selection.
        let mouse = |kind, column, row| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            })
        };
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut events = TestEventSource::new(vec![
            Event::Resize(80, 24),
            mouse(MouseEventKind::ScrollUp, 0, 0),
            mouse(MouseEventKind::ScrollDown, 0, 0),
            // Cursor motion without a button is not a gesture and must be
            // ignored rather than moving the view under the user.
            mouse(MouseEventKind::Moved, 0, 0),
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 15),
            mouse(MouseEventKind::Drag(MouseButton::Left), 20, 16),
            mouse(MouseEventKind::Up(MouseButton::Left), 20, 16),
            key(KeyCode::Char('q')),
        ]);

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
        // Tick 1: `poll_event` returns `None` (simulated poll-timeout - no input
        // pending); tick 2: q quits.  The `None` entry exercises the
        // `if let Some(event)` fallthrough path (line 127 in mod.rs).
        let mut dashboard = make_test_dashboard();
        let control = no_daemon_control();
        let mut terminal = test_terminal();
        // `None` entry → poll returns Ok(None) on tick 1 (no-event path);
        // `Some(q)` → poll returns Ok(Some(q)) on tick 2 → quit.
        let mut events = TestEventSource::new_with_nones(vec![None, Some(key(KeyCode::Char('q')))]);

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
        // ignored by the `_ => {}` arm; only the trailing q actually quits.
        let mut dashboard = make_test_dashboard();
        let control = no_daemon_control();
        let mut terminal = test_terminal();
        let release = Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            crossterm::event::KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        ));
        let mut events =
            TestEventSource::new(vec![release, Event::FocusGained, key(KeyCode::Char('q'))]);

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
    // polling from a test at all - so this doesn't get a test, matching
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
    // timer to fire - a future that never yields can't be preempted by a
    // sibling future racing it. In a non-TTY sandbox
    // `CrosstermEventSource::poll_event` fails immediately (so it looks
    // bounded in headless testing); on a real terminal, with no scripted key
    // ever setting `should_quit`, it hangs indefinitely.

    // `CrosstermEventSource`'s own poll/read branches and
    // `TestBackendHarness`'s delegated trait methods are covered where those
    // types now live, in `crate::tui`.

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
                let mut events = TestEventSource::new(vec![key(KeyCode::Char('q'))]);
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
                        let mut events = TestEventSource::new(vec![key(KeyCode::Char('q'))]);
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

    /// A prompt handed to `$EDITOR`: the loop returns, the editor runs with
    /// the terminal released, the dashboard takes it back and carries on.
    #[tokio::test]
    async fn execute_core_runs_the_editor_on_a_pending_prompt_and_carries_on() {
        crate::runstate::with_isolated_runs_dir_async(
            "execute_core_external_edit",
            |d| async move {
                let control = no_daemon_control();
                let mut dashboard = init_dashboard(control.clone(), |_| false);
                let path = d.join("prompt.md");
                std::fs::write(&path, "before").unwrap();
                dashboard.pending_external_edit = Some(agents::ExternalEdit {
                    path: path.clone(),
                    target: agents::PromptFocus::System,
                });
                let mut setup = TestSetup {
                    editor_writes: Some("after".to_string()),
                    ..TestSetup::new()
                };
                let mut events = TestEventSource::new(vec![key(KeyCode::Char('q'))]);
                let result = execute_core(&mut dashboard, &control, &mut setup, &mut events).await;
                assert!(result.is_ok());
                assert!(dashboard.should_quit);
                assert_eq!(setup.edited, vec![path.clone()]);
                // The file was read back (and removed) even with no editor
                // open to receive it.
                assert!(!path.exists());
                assert!(!dashboard.has_external_edit());
            },
        )
        .await;
    }

    /// Taking the terminal back after `$EDITOR` can fail like the first take.
    #[tokio::test]
    async fn execute_core_reports_a_failed_retake_after_the_editor() {
        crate::runstate::with_isolated_runs_dir_async(
            "execute_core_external_edit_retake",
            |d| async move {
                for (enable_on, create_on) in [(Some(2), None), (None, Some(2))] {
                    let control = no_daemon_control();
                    let mut dashboard = init_dashboard(control.clone(), |_| false);
                    let path = d.join("prompt.md");
                    std::fs::write(&path, "before").unwrap();
                    dashboard.pending_external_edit = Some(agents::ExternalEdit {
                        path: path.clone(),
                        target: agents::PromptFocus::Transition,
                    });
                    let mut setup = TestSetup {
                        enable_fails_on_call: enable_on,
                        create_fails_on_call: create_on,
                        ..TestSetup::new()
                    };
                    let mut events = TestEventSource::new(vec![key(KeyCode::Char('q'))]);
                    let result =
                        execute_core(&mut dashboard, &control, &mut setup, &mut events).await;
                    assert!(result.is_err());
                    assert_eq!(setup.edited.len(), 1);
                }
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
                    draw_should_fail: false,
                    ..TestSetup::new()
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
                // `enable()` succeeds, then `create_terminal()?` fails - deterministic
                // (no real backend / TTY involved), so this can never hang.
                let mut setup = TestSetup {
                    enable_should_fail: false,
                    create_should_fail: true,
                    draw_should_fail: false,
                    ..TestSetup::new()
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
