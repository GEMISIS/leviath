//! `lev integrate <host>`: register Leviath as an MCP server in a host agent
//! (Claude Code, Grok, Codex, Gemini or Hermes) and install the skill that
//! routes "use leviath to ..." to it.
//!
//! A host agent never picks Leviath on its own because `lev` is not in its
//! tool schema; it reaches for its own subagent tool instead. This command
//! puts `lev mcp serve` into the host's MCP config, drops a `SKILL.md` whose
//! description carries the trigger words, and installs the bundled blueprints
//! the server's default agent needs. Everything it touches is under the real
//! home directory (never `LEVIATH_HOME`), so every path and every process it
//! reaches is injected through [`IntegrateEnv`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::Args;

use crate::bundled::{install_bundled, plan_agent_actions};
use crate::config::Config;

pub(crate) mod hosts;
pub(crate) mod skill;

#[cfg(test)]
mod tests;

use hosts::{ALL_HOSTS, HostKind, integrate_host};

/// Which host to integrate with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Host {
    /// Claude Code (`~/.claude.json`, `~/.claude/skills`; honours `CLAUDE_CONFIG_DIR`)
    ClaudeCode,
    /// Grok Build (`~/.grok/config.toml`, `~/.grok/skills`)
    Grok,
    /// Codex CLI (`~/.codex/config.toml`, `~/.codex/skills`)
    Codex,
    /// Gemini CLI (`~/.gemini/settings.json`, `~/.gemini/skills`)
    Gemini,
    /// Hermes Agent (prints the `~/.hermes/config.yaml` snippet, writes `~/.hermes/skills`)
    Hermes,
    /// Every host whose dot-directory exists under the home directory
    All,
}

/// Arguments for `lev integrate`.
#[derive(Args, Debug)]
pub struct IntegrateArgs {
    /// The host agent to register Leviath in
    #[arg(value_enum)]
    host: Host,

    /// Register for this project only (Claude Code `.mcp.json` and
    /// `.claude/skills`; Grok `.grok/config.toml`) instead of for the user
    #[arg(long)]
    project: bool,

    /// Print what would be written or run, and write nothing
    #[arg(long)]
    print: bool,

    /// Register the server without installing the skill
    #[arg(long)]
    no_skill: bool,

    /// Do not install or update the bundled blueprints the server's default
    /// agent (`orchestrator`, with `coder` as its worker) runs
    #[arg(long)]
    no_agents: bool,
}

impl IntegrateArgs {
    /// A `claude-code` invocation with every flag off, for routing tests.
    #[cfg(test)]
    pub(crate) fn claude_code_for_test() -> Self {
        Self {
            host: Host::ClaudeCode,
            project: false,
            print: false,
            no_skill: false,
            no_agents: false,
        }
    }
}

/// A `PATH` lookup: the directory holding `bin`, or `None`.
pub type Which = Box<dyn Fn(&str) -> Option<PathBuf>>;

/// Run a program with arguments and return its stdout, or an error carrying
/// what it said on stderr.
pub type Run = Box<dyn Fn(&Path, &[String]) -> anyhow::Result<String>>;

/// Everything `lev integrate` reads from the machine, injected so the
/// command core writes only into a test's tempdir and runs nothing real.
pub struct IntegrateEnv {
    /// The real home directory (`dirs::home_dir`), never `LEVIATH_HOME`: the
    /// hosts keep their config under the former.
    pub home: PathBuf,
    /// `CLAUDE_CONFIG_DIR`, which replaces `~/.claude` (and moves
    /// `.claude.json` beside it) when set.
    pub claude_config_dir: Option<PathBuf>,
    /// The `lev` binary the hosts should launch (`std::env::current_exe`).
    pub lev_exe: PathBuf,
    /// The directory the command runs in, the root for `--project`.
    pub cwd: PathBuf,
    /// Where bundled blueprints are installed (`leviath_core::agents_dir`).
    pub agents_dir: Option<PathBuf>,
    /// Whether `[limits]` has neither `max_tool_call_write_bytes` nor
    /// `max_run_write_bytes`, in which case the next steps say to add them.
    pub limits_unset: bool,
    /// Whether the config holds a credential for at least one provider.
    pub providers_configured: bool,
    /// The `PATH` lookup used to find the `claude` CLI.
    pub which: Which,
    /// How a host CLI is run (`claude mcp add-json`).
    pub run: Run,
}

/// Whether the config sets no byte ceiling on what an agent may write.
pub fn limits_unset(config: &Config) -> bool {
    config.limits.max_tool_call_write_bytes.is_none() && config.limits.max_run_write_bytes.is_none()
}

/// Whether the config holds a credential for at least one provider, so a run
/// the host starts has a model to call.
pub fn providers_configured(config: &Config) -> bool {
    !crate::commands::setup::configured_providers(config).is_empty()
}

/// Find `bin` on `path` (the raw `PATH` value). Tries the bare name and the
/// Windows launcher suffixes, on every platform, so one code path serves all.
pub fn find_on_path(path: Option<OsString>, bin: &str) -> Option<PathBuf> {
    let path = path?;
    let names = [bin.to_string(), format!("{bin}.exe"), format!("{bin}.cmd")];
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|candidate| candidate.is_file())
}

/// What the command did, collected so tests read it and the terminal gets it
/// once, in order.
#[derive(Debug, Default)]
pub(crate) struct Report {
    lines: Vec<String>,
}

impl Report {
    fn push(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    /// A heading for one host.
    pub(crate) fn section(&mut self, label: &str) {
        self.push(format!("\n== {label} =="));
    }

    /// A line of plain information.
    pub(crate) fn note(&mut self, text: impl Into<String>) {
        self.push(format!("  {}", text.into()));
    }

    /// A file that was written.
    pub(crate) fn wrote(&mut self, path: &Path) {
        self.push(format!("  wrote {}", path.display()));
    }

    /// A file that already held exactly what would have been written.
    pub(crate) fn unchanged(&mut self, path: &Path) {
        self.push(format!("  unchanged {}", path.display()));
    }

    /// A file `--print` would have written, with its contents.
    pub(crate) fn would_write(&mut self, path: &Path, contents: &str) {
        self.push(format!("  would write {}:\n{contents}", path.display()));
    }

    /// The full text, one line per entry.
    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// Run `lev integrate` against the injected environment.
pub async fn execute_with(args: IntegrateArgs, env: &IntegrateEnv) -> anyhow::Result<()> {
    let report = integrate(&args, env)?;
    println!("{}", report.text());
    Ok(())
}

/// The hosts `host` names: one, or with `all` every host that is installed.
fn selected_hosts(host: Host, env: &IntegrateEnv) -> anyhow::Result<Vec<HostKind>> {
    let kinds = match host {
        Host::ClaudeCode => vec![HostKind::ClaudeCode],
        Host::Grok => vec![HostKind::Grok],
        Host::Codex => vec![HostKind::Codex],
        Host::Gemini => vec![HostKind::Gemini],
        Host::Hermes => vec![HostKind::Hermes],
        Host::All => ALL_HOSTS
            .into_iter()
            .filter(|kind| kind.dot_dir(env).is_dir())
            .collect(),
    };
    anyhow::ensure!(
        !kinds.is_empty(),
        "no host directory (.claude, .grok, .codex, .gemini, .hermes) under {}; name the host \
         instead: lev integrate <claude-code|grok|codex|gemini|hermes>",
        env.home.display()
    );
    Ok(kinds)
}

/// The tested core: every host, the bundled blueprints, then the next steps.
pub(crate) fn integrate(args: &IntegrateArgs, env: &IntegrateEnv) -> anyhow::Result<Report> {
    let hosts = selected_hosts(args.host, env)?;
    let mut report = Report::default();
    for host in &hosts {
        integrate_host(*host, args, env, &mut report)?;
    }
    if !args.no_agents {
        install_agents(args.print, env, &mut report);
    }
    next_steps(&hosts, env, &mut report);
    Ok(report)
}

/// Install or update the bundled blueprints, the way the headless `lev setup
/// --install-agents` does: a blueprint the user edited is left alone.
fn install_agents(print: bool, env: &IntegrateEnv, report: &mut Report) {
    report.section("bundled agents");
    let Some(agents_dir) = &env.agents_dir else {
        report.note("no agents directory could be resolved; run `lev setup --install-agents`");
        return;
    };
    let mut touched = 0;
    for (agent, action) in plan_agent_actions(agents_dir) {
        if !action.preselect() {
            continue;
        }
        touched += 1;
        let what = action.label(agent.version);
        if print {
            report.note(format!("would {what} {}", agent.name));
            continue;
        }
        match install_bundled(agent, agents_dir) {
            Ok(()) => report.note(format!("{what} {}", agent.name)),
            Err(e) => report.note(format!("could not install {}: {e}", agent.name)),
        }
    }
    if touched == 0 {
        report.note(format!("all up to date in {}", agents_dir.display()));
    }
}

/// The two write ceilings, as `docs/content/configuration.md` shows them.
const LIMITS_STANZA: &str = "[limits]\n\
max_tool_call_write_bytes = 2147483648   # 2 GiB; delete the line for no limit\n\
max_run_write_bytes       = 10737418240  # 10 GiB; delete the line for no limit";

/// What a person still has to do, printed last so it is what they see.
fn next_steps(hosts: &[HostKind], env: &IntegrateEnv, report: &mut Report) {
    report.section("next steps");
    let names: Vec<&str> = hosts.iter().map(|h| h.label()).collect();
    report.note(format!(
        "restart {} (or open a new session) so it loads the server and the skill, then say: \
         \"use leviath to <task>\"",
        names.join(", ")
    ));
    if hosts.contains(&HostKind::Hermes) {
        report.note(
            "hermes: paste the `mcp_servers:` snippet above into ~/.hermes/config.yaml and run \
             /reload-mcp; this command cannot do that step for you",
        );
    }
    if !env.providers_configured {
        report.note("no provider is configured yet: run `lev setup` before the first run");
    }
    if env.limits_unset {
        report.note(format!(
            "[limits] sets no byte ceiling, and an unattended run has no other one. Add this to \
             ~/.leviath/config.toml (lev setup's Limits screen writes the same keys):\n\n\
             {LIMITS_STANZA}\n"
        ));
    }
}
