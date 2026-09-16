//! `lev rage` - pack the logs and settings a bug report needs into one zip,
//! with every key removed.
//!
//! When something goes wrong, the useful evidence is spread over the config
//! file, the daemon's log, a run's journal and the blueprint that ran, and
//! a helper who gets one of them cannot reproduce anything. This builds one
//! `.zip` holding all of it, scrubbed of API keys, OAuth tokens and header
//! values (see `scrub`), and says in red what it still holds: the task, the
//! model's replies, tool output, file contents. It files nothing anywhere;
//! attaching the zip to an issue is the user's decision, made in a browser.
//!
//! A small TUI asks what the problem was about and, for a run, which one.
//! The flags answer the same questions for scripted use, and a stdout that
//! is not a terminal takes the flags path on its own.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, ValueEnum};
use crossterm::event::{Event, KeyEventKind};
use ratatui::Terminal;

use crate::tui::{EventSource, TerminalSetup};

mod archive;
mod collect;
mod render;
mod report;
mod scrub;
#[cfg(test)]
mod scrub_tests;
mod state;
#[cfg(test)]
mod tests;

pub(crate) use collect::Selection;
pub use collect::{Section, SkippedEntry};

/// The `--help` text past the first line.
pub const RAGE_LONG_ABOUT: &str = "\
Pack the logs and settings a bug report needs into one zip, with every key removed.

The zip holds `lev doctor --offline`, the daemon's state and log, the config
file with its keys taken out, every installed blueprint, and, for a run, the
run's metadata, stages, context, journal and blueprint. API keys, OAuth
tokens, header values and other credentials are removed. The task text, the
model's replies, tool output and file contents are kept: they are what a
helper needs. Read the zip before you share it.

With no flags a small screen asks what the problem was about. Every
question has a flag, and `--non-interactive` (or a stdout that is not a
terminal) builds the zip from the flags alone. Nothing is ever uploaded.";

/// What the problem was about, which decides what the bundle carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum About {
    /// Setting Leviath up: keys, providers, the wizard.
    Setup,
    /// A run that failed, hung or misbehaved.
    Run,
    /// Building a blueprint.
    Agent,
    /// Something else.
    Other,
}

/// Arguments for `lev rage`.
#[derive(Args, Debug, Clone, Default)]
#[command(long_about = RAGE_LONG_ABOUT)]
pub struct RageArgs {
    /// What the problem was about. Answers the first question on the screen.
    #[arg(long, value_enum)]
    pub about: Option<About>,

    /// The run it happened in: an exact id or a prefix only one run starts
    /// with. Implies `--about run`.
    #[arg(long, conflicts_with = "agent")]
    pub run: Option<String>,

    /// The blueprint you were building: its directory or its `agent.leviath`.
    /// Implies `--about agent`.
    #[arg(long)]
    pub agent: Option<PathBuf>,

    /// What happened, in your words. Lands at the top of the bundle's README.
    #[arg(long)]
    pub note: Option<String>,

    /// Where to write the zip. Default: `./leviath-rage-<timestamp>.zip`.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Leave a run's stored media parts out of the bundle.
    #[arg(long)]
    pub no_blobs: bool,

    /// No screen: build the zip from the flags and print its path.
    #[arg(long)]
    pub non_interactive: bool,
}

/// What the daemon looked like when the bundle was built. Filled by the
/// binary from the control socket without ever starting a daemon.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DaemonSnapshot {
    /// Whether something answered on the control socket.
    pub running: bool,
    /// The pid file's contents, if any.
    pub pid: Option<u32>,
    /// The build marker the last daemon wrote.
    pub build_on_disk: Option<String>,
    /// The build of the `lev` that made the bundle.
    pub cli_build: String,
    /// The daemon's own listing, when it answered.
    pub listing: Option<serde_json::Value>,
    /// What `lev daemon status` would have printed.
    pub status: Vec<String>,
    /// Why the listing is missing, when it is.
    pub note: Option<String>,
}

/// A read of the process environment by name, injected so a test controls it.
pub type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Everything from outside the process, injected so the whole command runs
/// under `cargo test` against a temp root.
pub struct RageEnv {
    /// `~/.leviath`.
    pub data_dir: PathBuf,
    /// The config file, wherever `LEVIATH_CONFIG_PATH` put it.
    pub config_path: PathBuf,
    /// `~/.leviath/runs`, or `LEVIATH_RUNS_DIR`.
    pub runs_dir: PathBuf,
    /// `~/.leviath/agents`.
    pub agents_dir: PathBuf,
    /// The directory holding `policy.toml` and `rules/`.
    pub policy_dir: PathBuf,
    /// The dashboard's activity log.
    pub dashboard_log: PathBuf,
    /// Where a bundle lands when `--output` is not given.
    pub cwd: PathBuf,
    /// Where other tools keep their configs, for the setup category.
    pub import_roots: crate::commands::setup::import::Roots,
    /// The process environment, by name.
    pub env_lookup: EnvLookup,
    /// Every environment variable name that is set.
    pub env_names: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    /// The daemon's state, read without starting one.
    pub daemon: Box<dyn Fn() -> DaemonSnapshot + Send + Sync>,
    /// How this `lev` was installed, in words.
    pub install: Box<dyn Fn() -> String + Send + Sync>,
    /// The clock, for the bundle's name and its timestamps.
    pub now: Box<dyn Fn() -> chrono::DateTime<chrono::Local> + Send + Sync>,
}

/// A bundle that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The zip.
    pub zip_path: PathBuf,
    /// Its size.
    pub zip_bytes: u64,
    /// What went in, by top-level directory.
    pub sections: Vec<Section>,
    /// What was left out, and why.
    pub skipped: Vec<SkippedEntry>,
    /// How many secrets were replaced across every member.
    pub redactions: usize,
    /// Things worth knowing that belong to no one file.
    pub notes: Vec<String>,
}

/// Gather and write the bundle `sel` describes.
pub(crate) async fn build(
    env: &RageEnv,
    sel: &Selection,
    output: Option<&Path>,
) -> anyhow::Result<Outcome> {
    let now = (env.now)();
    let root = format!("leviath-rage-{}", now.format("%Y%m%d-%H%M%S"));
    let zip_path = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env.cwd.join(format!("{root}.zip")));
    let bundle = collect::collect(env, sel, &now.to_rfc3339()).await;
    let zip_bytes = archive::write_zip(&bundle, &root, &zip_path)?;
    let sections = bundle.sections();
    let redactions = bundle.redactions();
    Ok(Outcome {
        zip_path,
        zip_bytes,
        sections,
        skipped: bundle.skipped,
        redactions,
        notes: bundle.notes,
    })
}

/// The selection the flags describe, for the path with no screen. A run
/// or a blueprint that the category needs and the flags do not name is a
/// refusal, since there is nobody to ask.
pub(crate) fn selection_from_args(args: &RageArgs, env: &RageEnv) -> anyhow::Result<Selection> {
    let about = args.about.unwrap_or(if args.run.is_some() {
        About::Run
    } else if args.agent.is_some() {
        About::Agent
    } else {
        About::Other
    });
    let run_id = match (about, &args.run) {
        (About::Run, Some(given)) => {
            let metas = collect::list_metas(&env.runs_dir);
            Some(collect::resolve_run_id(&metas, given).map_err(|e| anyhow::anyhow!(e))?)
        }
        (About::Run, None) => anyhow::bail!(
            "--about run needs the run: pass --run <id> (an exact id, or a prefix only one run starts with)"
        ),
        _ => None,
    };
    if about == About::Agent && args.agent.is_none() {
        anyhow::bail!("--about agent needs the blueprint: pass --agent <dir or agent.leviath>");
    }
    Ok(Selection {
        about,
        run_id,
        agent: args.agent.clone(),
        note: args.note.clone().unwrap_or_default(),
        include_blobs: !args.no_blobs,
    })
}

/// The flags path: no screen, the bundle from the flags, the path printed.
pub async fn run_non_interactive(args: &RageArgs, env: &RageEnv) -> anyhow::Result<()> {
    let sel = selection_from_args(args, env)?;
    let outcome = build(env, &sel, args.output.as_deref()).await?;
    print_outcome(&outcome);
    Ok(())
}

/// `lev rage`: the flags path, or the screen.
///
/// `is_terminal` is injected because it is a property of the real process's
/// stdout, and a screen that opens on a pipe takes over a terminal that is
/// not there. Unlike `lev setup`, a pipe is not refused: the flags path
/// works with no flags at all, so a script gets a bundle either way.
pub async fn execute_with<S: TerminalSetup, E: EventSource>(
    args: &RageArgs,
    env: &RageEnv,
    setup: &mut S,
    events: &mut E,
    is_terminal: bool,
) -> anyhow::Result<()> {
    if args.non_interactive || !is_terminal {
        return run_non_interactive(args, env).await;
    }
    let mut ui = state::Rage::new(args, env)?;
    match drive_screen(&mut ui, env, setup, events).await? {
        Some(outcome) => print_outcome(&outcome),
        None => println!("Cancelled. Nothing was written."),
    }
    Ok(())
}

/// Why the loop handed the terminal back.
pub(crate) enum LoopExit {
    /// The user left without a bundle.
    Quit,
    /// The summary step was reached and the bundle has to be built.
    Build,
    /// The summary was read; here is what was written.
    Finished(Outcome),
}

/// Take the terminal, run the screen, and hand the terminal back for the
/// build, then take it again for the summary.
///
/// The build runs between two takes because gathering the bundle runs
/// `lev doctor`'s config check, and a config with an odd-looking key makes
/// that print a warning straight to stderr, which is the same terminal the
/// screen is drawn on. Outside the alternate screen the line lands where a
/// warning belongs, and the summary is drawn clean afterwards.
async fn drive_screen<S: TerminalSetup, E: EventSource>(
    ui: &mut state::Rage,
    env: &RageEnv,
    setup: &mut S,
    events: &mut E,
) -> anyhow::Result<Option<Outcome>> {
    loop {
        setup.enable()?;
        let mut terminal = setup.create_terminal()?;
        let exit = run_loop(ui, &mut terminal, events, Duration::from_millis(120)).await;
        setup.disable();
        match exit? {
            LoopExit::Quit => return Ok(None),
            LoopExit::Finished(outcome) => return Ok(Some(outcome)),
            LoopExit::Build => match build(env, &ui.selection(), ui.output.as_deref()).await {
                Ok(outcome) => ui.outcome = Some(outcome),
                Err(e) => ui.error = Some(e.to_string()),
            },
        }
    }
}

/// Draw, read a key, repeat, until the screen is done, the user leaves, or
/// the summary step is reached with nothing built yet (see
/// [`drive_screen`] for why the build happens outside).
pub(crate) async fn run_loop<B: ratatui::backend::Backend>(
    ui: &mut state::Rage,
    terminal: &mut Terminal<B>,
    events: &mut impl EventSource,
    tick_rate: Duration,
) -> anyhow::Result<LoopExit> {
    loop {
        if ui.needs_build() {
            return Ok(LoopExit::Build);
        }
        terminal
            .draw(|frame| render::draw(frame, ui))
            .map_err(|e| anyhow::anyhow!("terminal draw failed: {e}"))?;

        if let Some(Event::Key(key)) = events.poll_event(tick_rate)?
            && key.kind == KeyEventKind::Press
        {
            ui.handle_key(key);
        }

        if ui.should_quit {
            return Ok(LoopExit::Quit);
        }
        if ui.finished {
            if let Some(error) = ui.error.take() {
                return Err(anyhow::anyhow!(error));
            }
            return Ok(LoopExit::Finished(
                ui.outcome
                    .take()
                    .expect("the summary is entered with an outcome or an error"),
            ));
        }
    }
}

/// What is printed once the terminal is handed back: where the zip is,
/// what was left out, and the warning that goes with it.
fn print_outcome(outcome: &Outcome) {
    println!("{}", outcome_lines(outcome).join("\n"));
}

/// The lines [`print_outcome`] prints, pure for the tests.
pub(crate) fn outcome_lines(outcome: &Outcome) -> Vec<String> {
    let mut lines = vec![format!(
        "Wrote {} ({}, {} secrets removed)",
        outcome.zip_path.display(),
        human_bytes(outcome.zip_bytes),
        outcome.redactions
    )];
    for section in &outcome.sections {
        lines.push(format!(
            "  {:<22} {:>4} file(s)  {:>10}",
            section.name,
            section.files,
            human_bytes(section.bytes)
        ));
    }
    for skipped in &outcome.skipped {
        lines.push(format!("  left out: {} ({})", skipped.path, skipped.reason));
    }
    for note in &outcome.notes {
        lines.push(format!("  note: {note}"));
    }
    lines.push(String::new());
    lines.push(format!("Before you share it: {}", report::PRIVACY_WARNING));
    lines.push("Read https://leviath.dev/docs/reporting-issues before you upload it.".to_string());
    lines
}

/// The directory `policy.toml` and `rules/` live in on this machine. For the
/// binary's real environment; the library reaches no real path on its own.
pub fn real_policy_dir() -> PathBuf {
    crate::commands::policy::policy_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// The dashboard's activity log on this machine.
pub fn real_dashboard_log_path() -> PathBuf {
    crate::runstate::dashboard_log_path()
}

/// How this `lev` was installed, in the words `lev update` uses.
pub fn real_install_description() -> String {
    use crate::commands::update::detect::{brew_prefix, detect};
    let exe = std::env::current_exe().unwrap_or_default();
    let home = leviath_core::paths::home_dir();
    detect(&exe, home.as_deref(), brew_prefix().as_deref(), None).describe()
}

/// `1.2 MiB`, `340 KiB`, `12 B`.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}
