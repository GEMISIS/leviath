//! `lev update` - bring this copy of Leviath up to date, then everything that
//! shipped with it.
//!
//! Three steps, in the order that matters: the binary, the bundled blueprints,
//! and the config file. The binary first because the other two are decided by
//! what the *new* binary ships, and a user who updates the agents against the
//! old one has done half a job.
//!
//! All three run every time. The binary step is never a reason to skip the
//! other two, and this is the whole point: `brew upgrade` and `scoop update`
//! hand over a new binary and say nothing about the blueprints in the user's
//! agents directory or the config beside them, so anyone who has ever updated
//! that way is carrying blueprints from whenever they last ran `lev setup`. A
//! binary that needs no update is not evidence that anything else is current,
//! which is why "already up to date" is a sentence this command never says on
//! its own.
//!
//! # Why the install method is detected rather than guessed
//!
//! Every channel installs the same `CARGO_PKG_VERSION`: the `-alpha` and
//! `-beta` suffixes live in the tap manifests the release workflow bumps, not
//! in `Cargo.toml`. So the version string says nothing about which channel this
//! binary came from, and nothing about which installer put it there. What does
//! say something is *where the file is*: a Homebrew Cellar path carries the
//! formula name, and the formula name carries the channel; a Scoop `apps` path
//! carries the package the same way; `~/.cargo/bin` means somebody compiled it.
//!
//! The hosted install script is the one method that records nothing. It writes
//! a plain binary into an ordinary directory and keeps no receipt, so a copy
//! that came from it is indistinguishable from any other loose binary. That is
//! what [`UpdateArgs::channel`] is for, and why the script arm defaults to
//! `stable` and says so rather than inferring a channel it cannot know.
//!
//! # Seams
//!
//! Running the upgrade and asking the user a question are both injected (see
//! [`UpdateEnv`]), so the tests assert the command that *would* run without a
//! single process being spawned - the same shape `lev mcp` uses for the browser
//! and the daemon uses for seed commands.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;

use crate::bundled::{AgentAction, BundledAgent, install_bundled, plan_agent_actions};
use crate::config::Config;

/// Where the hosted installer lives. One constant, because the invocation below
/// is easy to get subtly wrong and there must be exactly one copy of it.
const INSTALL_URL: &str = "https://leviath.dev/install.sh";

/// `lev update --help`.
pub const UPDATE_LONG_ABOUT: &str = "\
Update Leviath, then offer to bring everything else up to date with it.

The binary is updated with the installer that put it there, which is worked out
from where the file is rather than guessed from the version string (every
channel ships the same version number, so the string cannot tell you):

  Homebrew   a Cellar path names the formula, and the formula names the
             channel: `brew upgrade leviath-beta`
  Scoop      the same, from the package under scoop/apps
  cargo      says to run `cargo install leviath-cli` and stops. Updating it
             means a long compile, which is not something to start unasked
  script     re-runs the hosted installer for a channel. The install script
             keeps no record of the channel it used, so this defaults to
             stable - pass --channel to say otherwise

The blueprints and the config are checked every time, whatever the binary step
did. `brew upgrade` on its own leaves both behind, and a binary that was
already current is not a reason to stop looking: an install can be months
behind on its blueprints with a `lev` that needs no update at all.

Nothing is written to your agents directory without a yes. The whole list is
printed first, then one confirmation covers it; --install-agents is how a
script says yes. --yes alone is not enough, because updating a binary and
replacing the blueprints in your agents directory are different requests. A
copy you edited is named as edited and asked about on its own, and no flag
covers it: installing removes the directory and takes your edits with it.

Config migrations are described line by line before anything is written, and
then asked about.

--check and --json report the plan and change nothing. --dry-run walks the
whole flow, prompts and all, and prints what each step would do instead of
doing it.";

// ─── Channels ─────────────────────────────────────────────────────────────────

/// A release channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Channel {
    /// The weekly stable release. What crates.io and `brew install leviath` track.
    Stable,
    /// The promoted build a week ahead of stable.
    Beta,
    /// The nightly build.
    Alpha,
}

impl Channel {
    /// The name the install script and the docs use.
    pub fn id(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
        }
    }

    /// The Homebrew formula and Scoop package for this channel. They share a
    /// naming scheme on purpose, so one function answers for both.
    pub fn package(self) -> &'static str {
        match self {
            Self::Stable => "leviath",
            Self::Beta => "leviath-beta",
            Self::Alpha => "leviath-alpha",
        }
    }

    /// The channel a package name carries, or `None` for a name this build does
    /// not ship (someone's own formula, or one from a future channel).
    pub fn from_package(name: &str) -> Option<Self> {
        [Self::Stable, Self::Beta, Self::Alpha]
            .into_iter()
            .find(|c| c.package() == name)
    }
}

// ─── Install-method detection ─────────────────────────────────────────────────

/// How this copy of `lev` got onto the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallMethod {
    /// Homebrew, under the named formula.
    Homebrew {
        /// The formula, which is also what carries the channel.
        formula: String,
    },
    /// Scoop, under the named package.
    Scoop {
        /// The package, which carries the channel the same way a formula does.
        package: String,
    },
    /// `cargo install`, so the binary was compiled locally.
    Cargo,
    /// The hosted install script, or something else that dropped a plain binary
    /// where the install script puts one.
    Script {
        /// The channel to re-install. Never detected: see the module docs.
        channel: Channel,
    },
    /// Somewhere no supported installer writes.
    Unknown {
        /// Where the binary actually is, so the report can say it.
        path: PathBuf,
    },
}

impl InstallMethod {
    /// The short name used in `--json`.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Homebrew { .. } => "homebrew",
            Self::Scoop { .. } => "scoop",
            Self::Cargo => "cargo",
            Self::Script { .. } => "script",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// The channel this install tracks, where that is knowable.
    ///
    /// `cargo install leviath-cli` resolves crates.io, and each stable deploy
    /// publishes there from the same commit the binaries were built at, so a
    /// cargo install is a stable install by construction.
    pub fn channel(&self) -> Option<Channel> {
        match self {
            Self::Homebrew { formula } => Channel::from_package(formula),
            Self::Scoop { package } => Channel::from_package(package),
            Self::Cargo => Some(Channel::Stable),
            Self::Script { channel } => Some(*channel),
            Self::Unknown { .. } => None,
        }
    }

    /// The one-line description the report opens with.
    pub fn describe(&self) -> String {
        let channel = match self.channel() {
            Some(c) => format!(", {} channel", c.id()),
            // A formula this build does not ship, or a path nothing claims.
            None => String::new(),
        };
        match self {
            Self::Homebrew { formula } => format!("Homebrew (formula {formula}{channel})"),
            Self::Scoop { package } => format!("Scoop (package {package}{channel})"),
            Self::Cargo => format!("cargo install (crates.io{channel})"),
            Self::Script { .. } => format!("the install script ({INSTALL_URL}{channel})"),
            Self::Unknown { path } => {
                format!("something else - the binary is at {}", path.display())
            }
        }
    }
}

/// The component of `path` immediately after the first one equal to `marker`.
///
/// This is how both package managers record what they installed: Homebrew lays
/// a binary out as `<prefix>/Cellar/<formula>/<version>/bin/lev`, and Scoop as
/// `<root>/apps/<package>/current/lev.exe`. The name in that slot is the real
/// answer to "which channel is this", and it is on disk rather than inferred.
fn component_after(path: &Path, marker: &str) -> Option<String> {
    let mut components = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned());
    components.by_ref().find(|c| c == marker)?;
    components.next()
}

/// Whether `path` has a component equal to `marker`, ignoring case.
///
/// Scoop's root is a user-chosen directory that is conventionally but not
/// reliably lowercase, and Windows paths are case-insensitive anyway.
fn has_component(path: &Path, marker: &str) -> bool {
    path.components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(marker))
}

/// Homebrew prefixes that mean Homebrew and nothing else, for the case where
/// the binary is the `bin/lev` symlink rather than the Cellar path behind it.
///
/// `/usr/local` is deliberately absent even though it is Homebrew's own prefix
/// on Intel macOS: it is also where the install script and a hand-unpacked
/// tarball put a binary, so treating everything under it as Homebrew would send
/// a script install to `brew upgrade`. Under that prefix the Cellar component is
/// the only evidence that counts.
const UNAMBIGUOUS_BREW_PREFIXES: &[&str] = &["/opt/homebrew", "/home/linuxbrew/.linuxbrew"];

/// Absolute directories the installers write to. The Linux installer hard-codes
/// `/usr/local/bin`; `/usr/bin` is where the manual tarball instructions end up
/// for anyone who moved it there instead.
const SCRIPT_DESTINATIONS: &[&str] = &["/usr/local/bin", "/usr/bin"];

/// Every directory a plain-binary install lands in, including the two that are
/// home-relative: `~/.local/bin`, and the `%LOCALAPPDATA%\Leviath\bin` that
/// `install.ps1` writes on Windows.
///
/// A loose binary in one of these is a script install. A loose binary anywhere
/// else is not something to guess about, because re-running an installer aims
/// at a fixed destination and would leave the copy actually on `PATH` untouched.
fn script_destinations(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = SCRIPT_DESTINATIONS.iter().map(PathBuf::from).collect();
    if let Some(home) = home {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(
            home.join("AppData")
                .join("Local")
                .join("Leviath")
                .join("bin"),
        );
    }
    dirs
}

/// Work out how `exe` was installed.
///
/// Pure over its inputs - the resolved executable path, the home directory, the
/// answer `brew --prefix` gave (if it was asked and answered), and the channel
/// the user named - so every arm is testable without a Homebrew, a Scoop or a
/// second machine.
pub fn detect(
    exe: &Path,
    home: Option<&Path>,
    brew_prefix: Option<&Path>,
    requested: Option<Channel>,
) -> InstallMethod {
    // The channel to fall back on where the path does not name one.
    let channel = requested.unwrap_or(Channel::Stable);

    // Homebrew, from the strongest evidence down. A Cellar path names the
    // formula outright; a prefix only says "Homebrew put this here".
    if let Some(formula) = component_after(exe, "Cellar") {
        return InstallMethod::Homebrew { formula };
    }
    let under_brew = UNAMBIGUOUS_BREW_PREFIXES.iter().any(|p| exe.starts_with(p))
        || brew_prefix.is_some_and(|p| exe.starts_with(p) && !is_ambiguous_prefix(p));
    if under_brew {
        return InstallMethod::Homebrew {
            formula: channel.package().to_string(),
        };
    }

    // Scoop, the same two ways round.
    if has_component(exe, "scoop") {
        let package = component_after(exe, "apps").unwrap_or_else(|| channel.package().to_string());
        return InstallMethod::Scoop { package };
    }

    // A cargo install, which is the one method that cannot be updated in place.
    let cargo_bin = home.map(|h| h.join(".cargo").join("bin"));
    if cargo_bin.is_some_and(|dir| exe.starts_with(dir)) {
        return InstallMethod::Cargo;
    }

    let parent = exe.parent();
    let script_dir = script_destinations(home)
        .iter()
        .any(|d| parent == Some(d.as_path()));
    match script_dir {
        true => InstallMethod::Script { channel },
        false => InstallMethod::Unknown {
            path: exe.to_path_buf(),
        },
    }
}

/// Whether a prefix is too general to be evidence of anything on its own. See
/// [`UNAMBIGUOUS_BREW_PREFIXES`] for why `/usr/local` is the case that matters.
fn is_ambiguous_prefix(prefix: &Path) -> bool {
    matches!(
        prefix.to_string_lossy().trim_end_matches('/'),
        "/usr/local" | "/usr" | "" | "/"
    )
}

// ─── Config migrations ────────────────────────────────────────────────────────

/// One config change `lev update` knows how to make on the user's behalf.
///
/// The mechanism exists so that a future incompatibility - a key that moved, a
/// value whose meaning changed - is either fixed automatically or at least
/// explained at the moment the user updates into it, rather than surfacing as a
/// broken run days later. [`MIGRATIONS`] is empty today because no shipped
/// version has changed a key's name or meaning; the tests drive the machinery
/// with a sample so the wiring is proven rather than assumed.
pub struct Migration {
    /// A short stable name, shown in the report and in `--json`.
    pub name: &'static str,
    /// What it changes and why, in one line.
    pub description: &'static str,
    /// Whether this config needs it.
    ///
    /// Gets the parsed [`Config`] *and* the raw document, because the two see
    /// different things: a key serde no longer reads vanishes from the parsed
    /// value entirely, and a key that is still read but now means something
    /// else is only visible there.
    pub applies: fn(&Config, &toml::Table) -> bool,
    /// Make the change, returning one line per thing it did.
    pub apply: fn(&mut Config) -> Vec<String>,
}

/// Why `serves = []` is worth a migration at all.
///
/// It never meant anything. `serves` is the model list a script provider with no
/// `list_models` falls back to, and an empty one is the same as no entry - so the
/// line has always been inert. It got written because the field serialized even
/// when empty, and a save-back writes every field, so `lev setup` and every
/// config migration stamped it into each `[model_providers.*]` block.
///
/// Inert is not harmless once somebody is debugging. It reads as a declaration
/// that the provider serves nothing, which is exactly what a provider whose
/// `list_models` was never asked looks like from outside - so it got the blame
/// for a routing failure it had no part in. The real fault was priming, fixed
/// separately; this removes the thing that pointed at the wrong culprit.
///
/// The migrations this build knows about, oldest first.
///
/// Adding one is adding an entry here.
pub const MIGRATIONS: &[Migration] = &[Migration {
    name: "stale-empty-serves",
    description: "remove `serves = []` from [model_providers.*] - it never meant anything",
    applies: |config, _raw| {
        config
            .model_providers
            .values()
            .any(|p| p.serves.as_ref().is_some_and(Vec::is_empty))
    },
    apply: |config| {
        let mut done = Vec::new();
        for (name, provider) in &mut config.model_providers {
            if provider.serves.as_ref().is_some_and(Vec::is_empty) {
                provider.serves = None;
                done.push(format!(
                    "removed empty `serves` from [model_providers.{name}]"
                ));
            }
        }
        done
    },
}];

// ─── The plan ─────────────────────────────────────────────────────────────────

/// What to do about the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryStep {
    /// Run these, argv-style, in order, stopping at the first failure.
    ///
    /// A sequence rather than one command because a package manager will not
    /// see a release published minutes ago until its own index is refreshed,
    /// so the refresh and the upgrade are two commands that only make sense
    /// together.
    Run(Vec<Vec<String>>),
    /// There is nothing to run here. Tell the user this instead.
    Advise(String),
}

/// Everything `lev update` intends to do, as plain data.
///
/// Built before anything happens and rendered before anything happens, so the
/// report, the JSON and the actions can never disagree about what was planned.
pub struct UpdatePlan {
    /// How this copy was installed.
    pub method: InstallMethod,
    /// The binary step.
    pub binary: BinaryStep,
    /// Every bundled blueprint and what would happen to it.
    pub agents: Vec<(&'static BundledAgent, AgentAction)>,
    /// The migrations that apply to the config as it stands.
    pub migrations: Vec<&'static Migration>,
    /// What reading the config found.
    pub config: ConfigState,
}

/// What the plan found when it read the config file.
///
/// One value rather than a `Config` and an error beside it, because those two
/// only ever come in two of the four combinations and the other two would be
/// arms nothing could reach.
///
/// The config is carried rather than re-read at write time so there is exactly
/// one read: re-opening the file would add an error arm only a race could take,
/// and applying a migration to a document nobody has looked at since the report
/// was printed is exactly the surprise this command exists to avoid.
pub enum ConfigState {
    /// The config as it stands, for the migrations to be applied to. Boxed
    /// because a `Config` is far larger than the message beside it.
    Loaded(Box<Config>),
    /// It could not be read, and this is why.
    Unreadable(String),
}

/// One line naming every command a [`BinaryStep::Run`] will run, in order.
///
/// Joined with `&&` because that is both how the sequence behaves (each
/// command only runs if the one before it succeeded) and what a user would
/// paste into a shell to do it themselves.
pub fn render_commands(commands: &[Vec<String>]) -> String {
    commands
        .iter()
        .map(|argv| argv.join(" "))
        .collect::<Vec<_>>()
        .join(" && ")
}

/// The upgrade step for an install method: the commands to run, or the reason
/// there are none.
pub fn binary_step(method: &InstallMethod) -> BinaryStep {
    match method {
        // `brew update` first, every time. Homebrew upgrades against the tap
        // metadata it already has, so a formula published minutes ago is
        // invisible to `brew upgrade` on its own and the command cheerfully
        // reports the installed version as the latest. That is what sent
        // people to `brew update && brew upgrade leviath` by hand. The formula
        // name carries the channel, so this is the same two steps for
        // `leviath`, `leviath-alpha` and `leviath-beta`.
        InstallMethod::Homebrew { formula } => BinaryStep::Run(vec![
            vec!["brew".to_string(), "update".to_string()],
            vec!["brew".to_string(), "upgrade".to_string(), formula.clone()],
        ]),
        // Scoop has the same shape: a bare `scoop update` refreshes the
        // buckets, and `scoop update <app>` upgrades against whatever the
        // buckets already said.
        InstallMethod::Scoop { package } => BinaryStep::Run(vec![
            vec!["scoop".to_string(), "update".to_string()],
            vec!["scoop".to_string(), "update".to_string(), package.clone()],
        ]),
        // Deliberately not run. `cargo install` rebuilds the whole workspace
        // from source, which is minutes of CPU nobody asked for by typing
        // `lev update`.
        InstallMethod::Cargo => BinaryStep::Advise(
            "this copy was built by `cargo install`. Update it with \
             `cargo install leviath-cli` - that is a full compile, so it is not \
             something to start for you."
                .to_string(),
        ),
        // One shell, one pipeline. `LEVIATH_CHANNEL=beta curl ... | sh` is the
        // form to never generate: the assignment belongs to `curl`, the piped
        // shell never sees it, and the installer silently takes stable.
        InstallMethod::Script { channel } => BinaryStep::Run(vec![vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "curl -fsSL {INSTALL_URL} | sh -s -- --channel {}",
                channel.id()
            ),
        ]]),
        InstallMethod::Unknown { path } => BinaryStep::Advise(format!(
            "`lev` is at {}, which is not where any installer Leviath ships puts it. \
             Update it the way you installed it, or re-install with \
             `curl -fsSL {INSTALL_URL} | sh`.",
            path.display()
        )),
    }
}

/// The config as `lev update` needs to see it: parsed, and the document behind
/// it.
struct LoadedConfig {
    config: Config,
    raw: toml::Table,
}

/// Read the config file both ways.
fn load_config(path: &Path) -> anyhow::Result<LoadedConfig> {
    let config = Config::load_from_path_public(path)?;
    let raw = match std::fs::read_to_string(path) {
        // `expect`: `load_from_path_public` above parsed this same text as
        // TOML, so a document that reaches here is a document that parses.
        Ok(text) => toml::from_str::<toml::Table>(&text).expect("the config parsed a moment ago"),
        // No file at all, which loads as the defaults and an empty document.
        Err(_) => toml::Table::new(),
    };
    Ok(LoadedConfig { config, raw })
}

/// Work out everything the command would do, without doing any of it.
pub fn plan(args: &UpdateArgs, env: &UpdateEnv) -> UpdatePlan {
    let method = detect(
        &env.exe,
        env.home.as_deref(),
        env.brew_prefix.as_deref(),
        args.channel,
    );
    let binary = binary_step(&method);
    let agents = plan_agent_actions(&env.agents_dir);

    // A config that will not parse is reported, not fatal: the binary step is
    // the part of this command that matters most and it does not need one.
    let (migrations, config) = match load_config(&env.config_path) {
        Ok(loaded) => (
            env.migrations
                .iter()
                .filter(|m| (m.applies)(&loaded.config, &loaded.raw))
                .collect(),
            ConfigState::Loaded(Box::new(loaded.config)),
        ),
        Err(e) => (Vec::new(), ConfigState::Unreadable(e.to_string())),
    };

    UpdatePlan {
        method,
        binary,
        agents,
        migrations,
        config,
    }
}

// ─── Rendering ────────────────────────────────────────────────────────────────

/// The blueprints this plan would change.
fn changing(plan: &UpdatePlan) -> Vec<&(&'static BundledAgent, AgentAction)> {
    plan.agents.iter().filter(|(_, a)| a.is_change()).collect()
}

/// Render the plan the way `lev update` prints it.
///
/// Pure, so the tests assert the text exactly - everything that varies between
/// machines is already in the [`UpdatePlan`].
pub fn format_plan(plan: &UpdatePlan, version: &str) -> String {
    let mut out = format!(
        "\nlev {version}, installed with {}\n\n",
        plan.method.describe()
    );

    match &plan.binary {
        BinaryStep::Run(commands) => {
            out.push_str(&format!("  binary   {}\n", render_commands(commands)));
        }
        BinaryStep::Advise(text) => out.push_str(&format!("  binary   {text}\n")),
    }

    let changes = changing(plan);
    match changes.is_empty() {
        true => out.push_str(&format!(
            "  agents   all {} bundled blueprints are up to date\n",
            plan.agents.len()
        )),
        false => {
            out.push_str(&format!(
                "  agents   {} of {} would change\n",
                changes.len(),
                plan.agents.len()
            ));
            for (agent, action) in &changes {
                out.push_str(&format!(
                    "             {} - {}\n",
                    agent.name,
                    action.label(agent.version)
                ));
            }
        }
    }

    match (&plan.config, plan.migrations.is_empty()) {
        (ConfigState::Unreadable(e), _) => {
            out.push_str(&format!("  config   could not be read: {e}\n"))
        }
        (ConfigState::Loaded(_), true) => out.push_str("  config   nothing to migrate\n"),
        (ConfigState::Loaded(_), false) => {
            out.push_str(&format!(
                "  config   {} migration(s)\n",
                plan.migrations.len()
            ));
            for migration in &plan.migrations {
                out.push_str(&format!(
                    "             {} - {}\n",
                    migration.name, migration.description
                ));
            }
        }
    }
    out
}

/// The plan as JSON. Built by hand, like `lev tools` and `lev doctor`, so the
/// shape is explicit and does not move when a type gains a field.
pub fn plan_json(
    plan: &UpdatePlan,
    version: &str,
    latest: &latest::LatestCheck,
) -> serde_json::Value {
    let binary = match &plan.binary {
        // `commands` is a list of argv lists. The old `command` key held a
        // single argv and is kept alongside it, holding the last command (the
        // upgrade itself), so a script reading it still sees the step that
        // does the work rather than the index refresh in front of it.
        BinaryStep::Run(commands) => serde_json::json!({
            "action": "run",
            "commands": commands,
            "command": commands.last(),
        }),
        BinaryStep::Advise(text) => serde_json::json!({ "action": "advise", "message": text }),
    };
    let agents: Vec<serde_json::Value> = plan
        .agents
        .iter()
        .map(|(agent, action)| {
            serde_json::json!({
                "name": agent.name,
                "version": agent.version,
                "change": action.label(agent.version),
                "changes": action.is_change(),
                "preselected": action.preselect(),
            })
        })
        .collect();
    let migrations: Vec<serde_json::Value> = plan
        .migrations
        .iter()
        .map(|m| serde_json::json!({ "name": m.name, "description": m.description }))
        .collect();
    serde_json::json!({
        "version": version,
        "install_method": plan.method.id(),
        "channel": plan.method.channel().map(Channel::id),
        // All three together, and `null` together. "Not checked yet", "switched
        // off" and "the check failed" are one answer to a client - nothing to
        // show - and rendering that honestly as "can't tell" is what a console
        // already does for a channel it cannot judge.
        "latest": latest.latest,
        "update_available": latest.update_available,
        "checked_at": latest.checked_at,
        "binary": binary,
        "agents": agents,
        "migrations": migrations,
        "config_error": match &plan.config {
            ConfigState::Unreadable(e) => serde_json::Value::String(e.clone()),
            ConfigState::Loaded(_) => serde_json::Value::Null,
        },
    })
}

// ─── Arguments and seams ──────────────────────────────────────────────────────

/// Arguments for `lev update`.
#[derive(Args, Debug, Clone, Default)]
pub struct UpdateArgs {
    /// Report what would happen and change nothing.
    #[arg(long)]
    pub check: bool,

    /// Answer yes to the binary upgrade and the config write without asking.
    /// It deliberately does not install blueprints: see `--install-agents`.
    #[arg(long)]
    pub yes: bool,

    /// Install the bundled blueprints without asking. A copy you edited
    /// locally is still asked about on its own, since installing destroys it.
    #[arg(long)]
    pub install_agents: bool,

    /// The channel to install, for the install-script method, which records
    /// none. Ignored by Homebrew and Scoop, whose package name already says.
    #[arg(long, value_name = "CHANNEL")]
    pub channel: Option<Channel>,

    /// Walk the whole flow, but print each action instead of performing it.
    #[arg(long)]
    pub dry_run: bool,

    /// Print the plan as JSON and change nothing.
    #[arg(long)]
    pub json: bool,
}

/// Runs the upgrade command: argv in, success or the reason it failed out.
///
/// Injected rather than called directly so the tests assert *which* command
/// would run without spawning anything. Mirrors the `BrowserOpener` and
/// `SeedCommandRunner` seams.
pub type CommandRunner = Arc<dyn Fn(&[String]) -> anyhow::Result<()> + Send + Sync>;

/// Asks the user a yes/no question. Injected for the same reason: a unit test
/// has no terminal, and which questions a flag answers on the user's behalf -
/// and, for a blueprint they edited, which ones no flag answers - is something
/// the tests have to be able to prove rather than describe.
pub type Confirm = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// The real I/O `lev update` depends on, injected so the command logic is
/// testable without a package manager, a terminal, or the real home directory.
pub struct UpdateEnv {
    /// The running binary, symlinks already resolved. Resolving matters: a
    /// Homebrew `bin/lev` is a symlink into the Cellar, and the Cellar path is
    /// the one that names the formula.
    pub exe: PathBuf,
    /// The user's home directory, when one resolves.
    pub home: Option<PathBuf>,
    /// What `brew --prefix` answered, when it was asked and answered.
    pub brew_prefix: Option<PathBuf>,
    /// Where the bundled blueprints are installed.
    pub agents_dir: PathBuf,
    /// The config file to read and, with permission, rewrite.
    pub config_path: PathBuf,
    /// How to run the upgrade command.
    pub runner: CommandRunner,
    /// How to ask a yes/no question.
    pub confirm: Confirm,
    /// How to ask what the newest release on this channel is.
    ///
    /// A seam beside `runner` and `confirm` for the same reason: it reaches
    /// outside the process, so a test has to be able to answer for it. Also the
    /// switch for turning the check off - an air-gapped install passes one that
    /// declines, and every caller renders that as "can't tell" already.
    pub latest: latest::ReleaseFetcher,
    /// The migrations to consider, in order. Production passes [`MIGRATIONS`].
    pub migrations: &'static [Migration],
}

impl UpdateEnv {
    /// The real machine, wired up for a caller that only intends to [`plan`].
    ///
    /// `plan` never touches `runner` or `confirm` - it works out what the
    /// command *would* do and does none of it - so a caller that will not go on
    /// to `execute_with` has nothing to supply for them. Both are filled with
    /// refusals rather than no-ops, so a future caller that runs this env
    /// through the executing path fails loudly instead of silently doing
    /// nothing.
    ///
    /// Resolving the executable is the load-bearing part. A Homebrew `bin/lev`
    /// is a symlink into the Cellar, and the Cellar path is the only place the
    /// formula name - and so the channel - is written down, so the link has to
    /// be followed before [`detect`] can read anything off it.
    pub fn for_planning() -> Self {
        Self::real(
            std::sync::Arc::new(refuse_to_run),
            std::sync::Arc::new(say_no),
        )
    }

    /// [`Self::for_planning`], with the update check switched off.
    ///
    /// For a caller that must not touch the network on this path at all - the
    /// API route answers from a cache and refreshes elsewhere, so a fetcher
    /// that declines is the honest thing to hand it rather than one it is
    /// trusted not to call.
    pub fn for_planning_offline() -> Self {
        Self {
            latest: std::sync::Arc::new(decline_update_check),
            ..Self::for_planning()
        }
    }

    /// [`Self::real`], with the update check switched off when the config says
    /// so.
    ///
    /// Declining rather than skipping: every caller already renders "could not
    /// find out" honestly, so switching the check off and failing to reach the
    /// network land in the same place, and there is no second code path where
    /// one of them could behave differently.
    pub fn real_with_config(runner: CommandRunner, confirm: Confirm, update_check: bool) -> Self {
        match update_check {
            true => Self::real(runner, confirm),
            false => Self {
                latest: std::sync::Arc::new(decline_update_check),
                ..Self::real(runner, confirm)
            },
        }
    }

    /// The real machine, with the caller's own way to run a command and ask a
    /// question. `lev update` passes a terminal's; the API passes refusals.
    ///
    /// Infallible on purpose. Every step here degrades to worse *detection*
    /// rather than to an error: no home, no `brew`, or an executable path that
    /// will not resolve all land in [`detect`]'s `Unknown` arm, which advises
    /// re-installing. "I could not work out how you installed this" is an
    /// answer; refusing to answer is not, and a `lev update` that failed
    /// outright because it could not locate its own binary would be strictly
    /// less useful than one that says so.
    pub fn real(runner: CommandRunner, confirm: Confirm) -> Self {
        let home = dirs::home_dir();
        // A path that cannot be read, or cannot be canonicalized, is used as
        // whatever it came out as. An empty one detects as `Unknown`.
        let exe = std::env::current_exe().unwrap_or_default();
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        Self {
            exe,
            agents_dir: crate::commands::setup::real_agents_dir(home.as_deref()),
            home,
            brew_prefix: brew_prefix(),
            config_path: crate::config::Config::config_path(),
            runner,
            confirm,
            latest: std::sync::Arc::new(latest::fetch_release),
            migrations: MIGRATIONS,
        }
    }
}

/// The `runner` a planning-only environment carries: a loud refusal.
///
/// Not a no-op. A no-op runner would let a caller wire this env into
/// `execute_with`, watch it print every step, and change nothing - a silent
/// failure dressed as a successful update.
fn refuse_to_run(_argv: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("this update environment was built for planning only, and cannot run commands")
}

/// The `confirm` a planning-only environment carries. Nothing is agreed to by
/// something that is only working out what it would ask.
fn say_no(_question: &str) -> bool {
    false
}

/// What `brew --prefix` says, when there is a `brew` to ask.
///
/// Cached: this used to run once per `lev update`, and now also answers an HTTP
/// route, where spawning a process per request to learn a thing that cannot
/// change under a running server would be a waste worth noticing.
pub fn brew_prefix() -> Option<PathBuf> {
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            brew_prefix_from(
                leviath_sys::child_command("brew")
                    .arg("--prefix")
                    .output()
                    .ok(),
            )
        })
        .clone()
}

/// The part of [`brew_prefix`] that is worth testing: what to make of whatever
/// `brew --prefix` did or did not say.
///
/// Any failure is a `None` - it only ever adds evidence, and a machine without
/// Homebrew is the ordinary case rather than an error. Empty output is a `None`
/// for the same reason: an empty prefix would match every path under
/// [`detect`]'s `starts_with`, which is the opposite of no evidence.
///
/// Takes the answer rather than a closure that produces one. A generic seam
/// would be tidier to read and is measured per instantiation, so the arms this
/// machine's own `brew` does not take would go uncovered however many closures
/// the tests passed. The command is spawned once, inside the cache above, so
/// taking it eagerly costs nothing.
fn brew_prefix_from(output: Option<std::process::Output>) -> Option<PathBuf> {
    let output = output?;
    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix = prefix.trim();
    match prefix.is_empty() {
        true => None,
        false => Some(PathBuf::from(prefix)),
    }
}

// ─── Execution ────────────────────────────────────────────────────────────────

/// Ask, unless `--yes` has already answered.
fn agreed(args: &UpdateArgs, env: &UpdateEnv, question: &str) -> bool {
    args.yes || (env.confirm)(question)
}

/// Whether to offer the binary upgrade at all.
///
/// `false` only when the check positively says this copy is current: then
/// running a package manager over a binary with nothing to fetch is a prompt
/// somebody has to decline before reaching the blueprint and migration steps
/// they actually came for - which is the whole case for somebody who installed
/// the new binary their own way.
///
/// `None` is "could not find out", not "already current". The check may be
/// switched off, the network may be gone, or the copy may have no channel to
/// ask about. Offering the upgrade is the honest response to not knowing.
fn binary_step_needed(newest: &latest::LatestCheck) -> bool {
    newest.update_available != Some(false)
}

/// Step one: the binary.
fn update_binary(args: &UpdateArgs, env: &UpdateEnv, plan: &UpdatePlan) -> anyhow::Result<()> {
    let commands = match &plan.binary {
        BinaryStep::Advise(text) => {
            println!("  {text}");
            return Ok(());
        }
        BinaryStep::Run(commands) => commands,
    };
    // Asked about as one question, because they are one action: refreshing a
    // package index without then upgrading would be a strange thing to agree
    // to on its own.
    let shown = render_commands(commands);
    if !agreed(args, env, &format!("Run `{shown}`?")) {
        println!("  left the binary alone");
        return Ok(());
    }
    if args.dry_run {
        println!("  would run: {shown}");
        return Ok(());
    }
    for argv in commands {
        (env.runner)(argv)?;
    }
    Ok(())
}

/// Step two: the bundled blueprints.
///
/// Nothing here is a default. The whole list is printed first, and the agents
/// directory is not written to until someone has said yes to it: `--yes` is
/// deliberately not enough, because "update my binary without stopping to ask"
/// and "replace the blueprints in my agents directory" are different requests
/// and `lev setup` already treats them that way. `--install-agents` is how a
/// script says the second one.
///
/// A copy the user edited is named as edited and asked about on its own, and no
/// flag covers it: [`install_bundled`] removes the destination directory first,
/// so a bulk yes would take their edits and any file they added with it.
///
/// A failed install is a warning, not an abort, for the same reason `lev setup`
/// treats it that way: an updated binary plus most of the blueprints is a far
/// better place to leave someone than a command that gave up in the middle.
fn update_agents(args: &UpdateArgs, env: &UpdateEnv, plan: &UpdatePlan) {
    let changes = changing(plan);
    if changes.is_empty() {
        println!("  every bundled blueprint is already up to date");
        return;
    }

    // Seen before agreed to, always.
    for (agent, action) in &changes {
        println!("    {} - {}", agent.name, action.label(agent.version));
    }
    let clean = changes.iter().filter(|(_, a)| a.preselect()).count();
    let edited = changes.len() - clean;
    if edited > 0 {
        println!(
            "  {edited} of these you have edited locally. Installing removes the directory \
             first, so your edits and any file you added go with it - each is asked about \
             on its own."
        );
    }

    // One question for the whole clean set, rather than one per blueprint: the
    // list above is already the detail, and seven prompts in a row is how a
    // person stops reading them.
    let install_clean = match clean {
        0 => false,
        n => args.install_agents || (env.confirm)(&format!("Install these {n} blueprint(s)?")),
    };

    for (agent, action) in changes {
        let ok = match action.preselect() {
            true => install_clean,
            false => (env.confirm)(&format!(
                "{} - {}. Overwrite your edited copy?",
                agent.name,
                action.label(agent.version)
            )),
        };
        if !ok {
            println!("  skipped {}", agent.name);
            continue;
        }
        if args.dry_run {
            println!("  would install {} {}", agent.name, agent.version);
            continue;
        }
        match install_bundled(agent, &env.agents_dir) {
            Ok(()) => println!("  installed {} {}", agent.name, agent.version),
            Err(e) => println!("  could not install {}: {e}", agent.name),
        }
    }
}

/// Step three: the config.
///
/// Nothing is written before the user has seen, line by line, what changed.
fn migrate_config(args: &UpdateArgs, env: &UpdateEnv, plan: &UpdatePlan) -> anyhow::Result<()> {
    let config = match &plan.config {
        ConfigState::Unreadable(e) => {
            println!("  the config could not be read, so it was left alone: {e}");
            return Ok(());
        }
        ConfigState::Loaded(config) => config,
    };
    if plan.migrations.is_empty() {
        println!("  the config needs no changes");
        return Ok(());
    }

    let mut config = config.as_ref().clone();
    let mut changed = Vec::new();
    for migration in &plan.migrations {
        for line in (migration.apply)(&mut config) {
            changed.push(format!("{}: {line}", migration.name));
        }
    }
    for line in &changed {
        println!("    - {line}");
    }

    let path = env.config_path.display();
    if !agreed(args, env, &format!("Write these changes to {path}?")) {
        println!("  config left as it is");
        return Ok(());
    }
    if args.dry_run {
        println!("  would write {path}");
        return Ok(());
    }
    config.save_to_path_public(&env.config_path)?;
    println!("  wrote {path}");
    Ok(())
}

/// A fetcher that does not look anything up.
///
/// Used both by the path that must not touch the network and by an install that
/// has turned the check off. One function for both, because "did not ask" is a
/// single outcome and a caller cannot act on which reason it was.
fn decline_update_check(_: &str) -> Result<String, String> {
    Err("the update check is not enabled on this path".to_string())
}

/// Ask what the newest release on this copy's channel is.
///
/// A copy whose channel could not be worked out is not asked about: the answer
/// would be the stable release compared against a build that may not be on that
/// line at all, which is exactly the wrong-in-both-directions guess the console
/// was making before any of this (issue #600).
fn check_latest(plan: &UpdatePlan, env: &UpdateEnv, version: &str) -> latest::LatestCheck {
    match plan.method.channel() {
        Some(channel) => latest::check_with(channel, version, &env.latest, latest::now_secs()),
        None => latest::LatestCheck::default(),
    }
}

/// The one line `lev update` prints about whether the update is worth doing.
///
/// Silent when there is nothing to say. A check that could not run prints
/// nothing rather than "could not check for updates": the command was asked how
/// to update, it has answered that, and a failed lookup of a thing the user did
/// not ask for is noise on a terminal.
fn format_latest(newest: &latest::LatestCheck, running: &str) -> String {
    match (newest.update_available, &newest.latest) {
        (Some(true), Some(latest)) => {
            format!("\n{latest} is available (you have {running}).\n")
        }
        (Some(false), _) => format!("\n{running} is the newest on this channel.\n"),
        _ => String::new(),
    }
}

/// Run `lev update` against an injected environment.
pub fn execute_with(args: &UpdateArgs, env: &UpdateEnv, version: &str) -> anyhow::Result<()> {
    let plan = plan(args, env);
    let newest = check_latest(&plan, env, version);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan_json(&plan, version, &newest))
                .expect("a plan is plain data and always serializes")
        );
        return Ok(());
    }

    print!("{}", format_plan(&plan, version));
    print!("{}", format_latest(&newest, version));
    if args.check {
        return Ok(());
    }

    println!("\nbinary");
    match binary_step_needed(&newest) {
        true => update_binary(args, env, &plan)?,
        false => println!("  {version} is the newest on this channel"),
    }
    println!("\nblueprints");
    update_agents(args, env, &plan);
    println!("\nconfig");
    migrate_config(args, env, &plan)?;
    Ok(())
}

pub mod latest;

#[cfg(test)]
mod tests;
