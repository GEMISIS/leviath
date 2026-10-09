//! The commands that take over the real terminal (`lev dash`, `lev setup`,
//! `lev rage`), and the crossterm [`TerminalSetup`] they all share.

use std::fs::File;
use std::io;

use crossterm::ExecutableCommand;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::daemon::{ensure_daemon_running, resolve_service_unit};
use super::{control_client, long_lived_control_client};
use crate::commands;
use crate::commands::dashboard::{CrosstermEventSource, DashboardArgs, TerminalSetup};

/// Real `lev dash`: supplies the real crossterm terminal backend and event
/// source to the library's fully-tested `dashboard::execute_with`. Wiring
/// only - the loop, rendering, input handling, and engine setup it composes
/// are all exercised under `cargo test`.
pub(super) async fn real_dashboard(_args: DashboardArgs) -> anyhow::Result<()> {
    // The dashboard is a client of the shared-world daemon: ensure it's running,
    // then observe/control it over the control socket.
    ensure_daemon_running().await?;
    let control = long_lived_control_client()?;
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
        mouse_capture: true,
        enabled: false,
    };
    let mut events = CrosstermEventSource::open();
    commands::dashboard::execute_with(control, &mut setup, &mut events, real_yank).await
}

/// Real clipboard copy for the dashboard's `y` keypress: try a native tool,
/// then fall back to writing the OSC52 escape sequence to the real controlling
/// terminal / stdout. The native-tool + fallback branch logic is unit-tested in
/// `yank_to_clipboard_via`; `leviath_sys::osc52_write_via`'s branches are
/// unit-tested via injected fakes. The two real-I/O leaves it composes here -
/// opening `/dev/tty` and acquiring `stdout()` - are the un-unit-testable slivers.
fn real_yank(text: &str) -> bool {
    commands::dashboard::yank_to_clipboard_via(text, |t| {
        let mut out = io::stdout();
        leviath_sys::osc52_write_via(t, open_controlling_tty, &mut out)
    })
}

/// Open the process's controlling terminal (`/dev/tty` on Unix) for writing the
/// OSC52 clipboard escape sequence. Errors on non-Unix, where `real_yank` then
/// falls back to stdout.
#[cfg(unix)]
fn open_controlling_tty() -> io::Result<File> {
    std::fs::OpenOptions::new().write(true).open("/dev/tty")
}

#[cfg(not(unix))]
fn open_controlling_tty() -> io::Result<File> {
    Err(io::Error::other("no controlling terminal on this platform"))
}

/// Real `lev setup`: the real config paths, the real environment, a real
/// browser for the "open the signup page" key, a real TTY check, and the real
/// network-backed provider verifier - everything the library's tested
/// `execute_with` takes as a seam.
///
/// The verification task is spawned here rather than inside `execute_with`
/// because `LiveVerifier` is the one piece that opens a socket; the library
/// never instantiates it, so no unit test can reach the network through it.
pub(super) async fn real_setup(args: commands::setup::SetupArgs) -> anyhow::Result<()> {
    use crate::commands::setup::signin::{LiveAuthorizer, signin_loop};
    use crate::commands::setup::verify::{LiveVerifier, SkipVerifier};
    use commands::setup::{SetupEnv, import, verification_loop};

    let home = crate::config::leviath_home_dir().unwrap_or_default();
    let env = SetupEnv {
        config_path: crate::config::Config::config_path(),
        agents_dir: commands::setup::real_agents_dir(Some(&home)),
        roots: import::Roots::new(
            home,
            dirs::config_dir().unwrap_or_default(),
            std::env::current_dir().unwrap_or_default(),
        ),
        env_lookup: Box::new(|name| std::env::var(name).ok()),
        opener: std::sync::Arc::new(leviath_sys::open_url),
        // Shared with the dashboard, so an import somebody has already turned
        // down is not proposed again the next time they run setup.
        ui_state_path: crate::ui_state::default_path(),
    };
    if args.non_interactive {
        return commands::setup::run_non_interactive(&args, &env);
    }
    if !std::io::IsTerminal::is_terminal(&io::stdout()) {
        return commands::setup::execute_with(
            &args,
            &env,
            &mut CrosstermSetup {
                viewport: Viewport::Fullscreen,
                mouse_capture: false,
                enabled: false,
            },
            &mut CrosstermEventSource::open(),
            false,
        )
        .await;
    }

    let mut wizard = commands::setup::build_wizard(&env);
    if let Some((requests, replies)) = wizard.take_verify_ends() {
        if args.no_verify {
            tokio::spawn(verification_loop(SkipVerifier, requests, replies));
        } else {
            tokio::spawn(verification_loop(LiveVerifier, requests, replies));
        }
    }
    if let Some((requests, events)) = wizard.take_signin_ends() {
        let authorizer = LiveAuthorizer::real(env.opener.clone(), &env.config_path);
        tokio::spawn(signin_loop(authorizer, requests, events));
    }
    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
        mouse_capture: true,
        enabled: false,
    };
    let mut events = CrosstermEventSource::open();
    commands::setup::execute_core(&mut wizard, &env, &mut setup, &mut events).await
}

/// Real `lev rage`: the real paths, the real environment, a daemon probe
/// over the control socket that never starts one, and the real terminal.
/// Everything it composes is the library's tested `execute_with`.
pub(super) async fn real_rage(args: commands::rage::RageArgs) -> anyhow::Result<()> {
    use commands::rage::{DaemonSnapshot, RageEnv};
    use commands::setup::import;

    let home = crate::config::leviath_home_dir().unwrap_or_default();
    let data_dir = leviath_core::paths::data_dir().unwrap_or_default();
    let env = RageEnv {
        config_path: crate::config::Config::config_path(),
        runs_dir: crate::runstate::runs_dir(),
        agents_dir: commands::setup::real_agents_dir(Some(&home)),
        policy_dir: commands::rage::real_policy_dir(),
        dashboard_log: commands::rage::real_dashboard_log_path(),
        cwd: std::env::current_dir().unwrap_or_default(),
        import_roots: import::Roots::new(
            home,
            dirs::config_dir().unwrap_or_default(),
            std::env::current_dir().unwrap_or_default(),
        ),
        env_lookup: Box::new(|name| std::env::var(name).ok()),
        env_names: Box::new(|| std::env::vars().map(|(name, _)| name).collect()),
        daemon: Box::new(real_daemon_snapshot),
        install: Box::new(commands::rage::real_install_description),
        now: Box::new(chrono::Local::now),
        data_dir,
    };
    // The probe below is synchronous over an async client, so it runs on a
    // thread of its own rather than blocking this runtime's worker.
    fn real_daemon_snapshot() -> DaemonSnapshot {
        use leviath_runtime::control_socket::{ControlToken, is_daemon_running};
        let mut snapshot = DaemonSnapshot {
            cli_build: crate::daemon::setup::CURRENT_BUILD.to_string(),
            build_on_disk: crate::daemon::setup::read_build_marker(),
            ..DaemonSnapshot::default()
        };
        let Some(id) = crate::daemon::setup::control_address() else {
            snapshot.note = Some("no home directory, so no control socket".to_string());
            return snapshot;
        };
        if let Some(dir) = crate::daemon::setup::control_dir() {
            snapshot.pid = ControlToken::read_pid(&dir);
        }
        snapshot.running = is_daemon_running(&id);
        let mut count = 0;
        if snapshot.running {
            let listing = std::thread::spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
                    .and_then(|rt| {
                        rt.block_on(async {
                            control_client()
                                .map_err(|e| e.to_string())?
                                .list()
                                .await
                                .map_err(|e| e.to_string())
                        })
                    })
            })
            .join()
            .unwrap_or_else(|_| Err("the probe thread panicked".to_string()));
            match listing {
                Ok(response) => {
                    if let leviath_runtime::control_socket::ControlResponse::List { runs, .. } =
                        &response
                    {
                        count = runs.len();
                    }
                    snapshot.listing = serde_json::to_value(&response).ok();
                }
                Err(e) => snapshot.note = Some(format!("the daemon did not answer: {e}")),
            }
        }
        let supervision = resolve_service_unit().ok().map(|unit| {
            commands::daemon_service::format_supervision(unit.path.exists(), &unit.path)
        });
        snapshot.status =
            crate::daemon::lifecycle::status_lines(snapshot.running, count, supervision);
        snapshot
    }

    let mut setup = CrosstermSetup {
        viewport: Viewport::Fullscreen,
        mouse_capture: false,
        enabled: false,
    };
    let mut events = CrosstermEventSource::open();
    let is_terminal = std::io::IsTerminal::is_terminal(&io::stdout());
    commands::rage::execute_with(&args, &env, &mut setup, &mut events, is_terminal).await
}

/// Real [`TerminalSetup`]: enables raw mode, enters/leaves the real alternate
/// screen, and builds a real `CrosstermBackend` on `stdout`. Lives in the
/// binary because it can only be exercised against a real terminal.
///
/// `mouse_capture` is per-surface: the dashboard wants wheel events and its
/// own click-drag selection, and the setup wizard wants clicks on its rows and
/// buttons. It stays off for the workdir prompt, which is one question with
/// two answers and nothing to aim at.
pub(super) struct CrosstermSetup {
    pub(super) viewport: Viewport,
    pub(super) mouse_capture: bool,
    /// True between a successful `enable()` and the matching `disable()`, so
    /// teardown (explicit, `Drop`, or the panic hook) runs exactly once.
    pub(super) enabled: bool,
}

/// Restore the terminal from raw mode / alternate screen / mouse capture.
/// Safe to call redundantly: every step is a no-op when already released.
fn restore_terminal() {
    io::stdout().execute(PopKeyboardEnhancementFlags).ok();
    io::stdout().execute(DisableMouseCapture).ok();
    disable_raw_mode().ok();
    io::stdout().execute(LeaveAlternateScreen).ok();
}

/// Whether a `CrosstermSetup` currently holds the terminal; read by the panic
/// hook so a panic after clean teardown doesn't re-issue restore sequences
/// into a healthy shell.
static TERMINAL_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install (once) a chained panic hook that restores the terminal *before*
/// the default hook prints the panic message - otherwise a panic mid-loop
/// leaves the shell in raw mode with the message drawn into the vanished
/// alternate screen.
fn install_terminal_restore_panic_hook() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TERMINAL_HELD.load(std::sync::atomic::Ordering::SeqCst) {
                restore_terminal();
                // The screen is back, so parked lines can be shown, and the
                // panic message that follows is not competing with the frame.
                // Clearing the flag also stops a loop still running on another
                // thread from re-parking output nobody will ever flush.
                TERMINAL_HELD.store(false, std::sync::atomic::Ordering::SeqCst);
                crate::logging::release_from_tui();
            }
            previous(info);
        }));
    });
}

impl TerminalSetup for CrosstermSetup {
    type B = ratatui::backend::CrosstermBackend<io::Stdout>;

    fn run_editor(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        leviath_sys::editor::launch(path)
    }

    fn enable(&mut self) -> anyhow::Result<()> {
        install_terminal_restore_panic_hook();
        enable_raw_mode().map_err(anyhow::Error::from)?;
        // The kitty keyboard protocol is what lets Ctrl+Enter (start the run
        // from the new-run task) arrive as anything other than Enter. Asked
        // for only where the terminal says it can, and popped again in
        // `restore_terminal`.
        if matches!(
            crossterm::terminal::supports_keyboard_enhancement(),
            Ok(true)
        ) {
            io::stdout()
                .execute(PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
                ))
                .map_err(anyhow::Error::from)?;
        }
        io::stdout()
            .execute(EnterAlternateScreen)
            .map_err(anyhow::Error::from)?;
        if self.mouse_capture {
            // Mouse capture is what delivers wheel events, and it routes
            // click-drag to the dashboard's own text selection (drag to
            // highlight, release to copy). Hold Shift (or Option on macOS
            // Terminal) to bypass capture and use the terminal's native
            // selection instead.
            io::stdout()
                .execute(EnableMouseCapture)
                .map_err(anyhow::Error::from)?;
        }
        self.enabled = true;
        TERMINAL_HELD.store(true, std::sync::atomic::Ordering::SeqCst);
        // stderr is this same terminal, so a log line from here on would land
        // inside the frame - and in raw mode, without a carriage return,
        // staircase across it. Park them until the screen is ours again.
        crate::logging::hold_for_tui();
        Ok(())
    }

    fn create_terminal(&mut self) -> anyhow::Result<Terminal<Self::B>> {
        let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
        Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: self.viewport.clone(),
            },
        )
        .map_err(anyhow::Error::from)
    }

    fn disable(&mut self) {
        if !self.enabled {
            return;
        }
        self.enabled = false;
        TERMINAL_HELD.store(false, std::sync::atomic::Ordering::SeqCst);
        // Flushes whatever was logged while the screen was held, so `-v`
        // diagnostics are waiting in the scrollback rather than lost.
        crate::logging::release_from_tui();
        // Mouse release runs before leaving the alternate screen, and
        // unconditionally (even when capture was never enabled - it's a
        // no-op then): a terminal left in mouse-reporting mode emits escape
        // sequences into the user's shell on every click afterwards.
        restore_terminal();
    }

    fn print_done(&self) {
        println!("Dashboard closed.");
    }
}

/// Covers early-return paths (`?` between `enable()` and the loop's own
/// teardown): the terminal is restored on unwind, and the `enabled` guard
/// makes the common explicit-`disable()`-then-drop sequence a single restore.
impl Drop for CrosstermSetup {
    fn drop(&mut self) {
        self.disable();
    }
}
