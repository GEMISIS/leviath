//! `lev deps` - inspect and install a blueprint's declared dependencies.
//!
//! `lev deps list <agent>` shows what an agent declares it needs; `lev deps
//! check <agent>` says whether this machine has it (and exits non-zero when a
//! required one is missing); `lev deps install <agent>` puts them in place,
//! always after asking, because installing runs commands and writes config on
//! the machine.
//!
//! The command logic lives in [`execute_with`], driven by a [`DepsEnv`] whose
//! seams - the config path, the machine [`Probe`], a [`CommandRunner`] and a
//! [`Prompt`] - are injected so the whole thing is testable without touching
//! the real machine. `main.rs` builds the real env.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use leviath_core::blueprint::{Blueprint, Dependency, DependencyInstall, DependencyKind};
use leviath_mcp::{MCPServerConfig, MCPTransport};

use crate::dependencies::{self, Probe};

/// `lev deps` and its subcommands.
#[derive(clap::Args, Debug)]
pub struct DepsArgs {
    /// Which dependency action to take.
    #[command(subcommand)]
    pub command: DepsCommand,
}

/// The `lev deps` subcommands.
#[derive(clap::Subcommand, Debug)]
pub enum DepsCommand {
    /// List the dependencies an agent declares.
    List(AgentRef),
    /// Check whether this machine satisfies an agent's dependencies.
    Check(AgentRef),
    /// Install an agent's dependencies (runs commands and writes config).
    Install(InstallArgs),
}

/// An installed agent name, or a path to a blueprint directory or manifest.
#[derive(clap::Args, Debug)]
pub struct AgentRef {
    /// An installed agent name, or a path to its directory or `agent.leviath`.
    pub agent: String,
}

/// Arguments for `lev deps install`.
#[derive(clap::Args, Debug)]
pub struct InstallArgs {
    /// An installed agent name, or a path to its directory or `agent.leviath`.
    pub agent: String,
    /// Install without the per-dependency confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Act on every dependency, not only the ones that are missing.
    #[arg(long)]
    pub all: bool,
}

/// Runs a shell command during install. Injected so tests never shell out.
pub trait CommandRunner: Send + Sync {
    /// Run a command, returning `Ok` on success or an error message.
    fn run(&self, command: &str) -> Result<(), String>;
}

/// Asks the user things during install. Injected so tests never read stdin.
pub trait Prompt {
    /// Ask a yes/no question; the default when unsure is "no".
    fn confirm(&self, message: &str) -> bool;
}

/// The seams `lev deps` needs, so [`execute_with`] is testable end to end.
pub struct DepsEnv {
    /// Where the user's `config.toml` lives (read for servers, written on install).
    pub config_path: PathBuf,
    /// The installed-agents directory, for resolving an agent by name.
    pub agents_dir: Option<PathBuf>,
    /// Reads env and `PATH`.
    pub probe: Box<dyn Probe>,
    /// Runs install commands.
    pub runner: Arc<dyn CommandRunner>,
    /// Asks the user to confirm.
    pub prompt: Box<dyn Prompt>,
    /// This host's OS key for per-OS install commands: `macos`/`linux`/`windows`.
    pub os: &'static str,
}

/// The OS key used for per-OS install commands on the host this build runs on.
///
/// `#[cfg]` rather than `cfg!()` so only the arm for the target platform is
/// compiled - the other two are not instrumented, so each platform's build
/// covers the one branch it can take.
pub fn host_os() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "macos"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "linux"
    }
}

/// Run a `lev deps` subcommand against the injected environment.
pub fn execute_with(args: DepsArgs, env: &DepsEnv) -> anyhow::Result<()> {
    match args.command {
        DepsCommand::List(a) => list(&a.agent, env),
        DepsCommand::Check(a) => check(&a.agent, env),
        DepsCommand::Install(a) => install(a, env),
    }
}

/// Resolve an agent name-or-path to its parsed blueprint and its directory.
///
/// The read is the existence check (an agent that resolves to no readable
/// manifest is "not found"), so there is one error path per real failure -
/// missing, unparseable, invalid - and no unreachable guard.
fn resolve_agent(agent: &str, env: &DepsEnv) -> anyhow::Result<(Blueprint, PathBuf)> {
    let as_path = Path::new(agent);
    let (manifest, dir) = if as_path.is_file() {
        let dir = as_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        (as_path.to_path_buf(), dir)
    } else if as_path.is_dir() {
        (
            as_path.join(leviath_core::files::MANIFEST_FILENAME),
            as_path.to_path_buf(),
        )
    } else if let Some(dir) = &env.agents_dir {
        let base = dir.join(agent);
        (base.join(leviath_core::files::MANIFEST_FILENAME), base)
    } else {
        bail!("no such path '{agent}', and no installed-agents directory to look it up in");
    };
    let content = std::fs::read_to_string(&manifest).map_err(|_| {
        anyhow::anyhow!(
            "could not find agent '{agent}': no manifest at {}",
            manifest.display()
        )
    })?;
    let blueprint = leviath_core::manifest::parse_manifest(&content)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", manifest.display()))?;
    blueprint
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid blueprint {}: {e}", manifest.display()))?;
    Ok((blueprint, dir))
}

/// The configured MCP servers, or an empty list when there is no config yet.
fn configured_servers(env: &DepsEnv) -> anyhow::Result<Vec<MCPServerConfig>> {
    Ok(load_config_or_default(&env.config_path)?.mcp_servers)
}

/// Load the config at `path`, or the defaults when the file does not exist.
fn load_config_or_default(path: &Path) -> anyhow::Result<crate::config::Config> {
    if path.is_file() {
        crate::config::Config::load_from_path_public(path)
    } else {
        Ok(crate::config::Config::default())
    }
}

fn list(agent: &str, env: &DepsEnv) -> anyhow::Result<()> {
    let (blueprint, _dir) = resolve_agent(agent, env)?;
    if blueprint.dependencies.is_empty() {
        println!("'{}' declares no dependencies.", blueprint.name);
        return Ok(());
    }
    println!("'{}' dependencies:", blueprint.name);
    for dep in &blueprint.dependencies {
        let req = if dep.required { "required" } else { "optional" };
        println!("  - {} ({}, {req})", dep.name, dep.kind.tag());
        println!("      {}", describe_kind(&dep.kind));
        if let Some(desc) = &dep.description {
            println!("      {desc}");
        }
        if dep.install.is_some() {
            println!("      installable with `lev deps install {agent}`");
        }
    }
    Ok(())
}

/// A one-line "what it wants" for a dependency kind.
fn describe_kind(kind: &DependencyKind) -> String {
    match kind {
        DependencyKind::McpServer { server, env } if env.is_empty() => {
            format!("MCP server '{server}'")
        }
        DependencyKind::McpServer { server, env } => {
            format!("MCP server '{server}' with {}", env.join(", "))
        }
        DependencyKind::Env { var } => format!("environment variable {var}"),
        DependencyKind::Binary { command } => format!("program '{command}' on PATH"),
        DependencyKind::Script { check } => format!("script check {check}"),
    }
}

fn check(agent: &str, env: &DepsEnv) -> anyhow::Result<()> {
    let (blueprint, dir) = resolve_agent(agent, env)?;
    let servers = configured_servers(env)?;
    let report =
        dependencies::evaluate(&blueprint.dependencies, &servers, &dir, env.probe.as_ref());
    if report.statuses.is_empty() {
        println!("'{}' declares no dependencies.", blueprint.name);
        return Ok(());
    }
    println!("'{}' dependencies:", blueprint.name);
    for s in &report.statuses {
        println!("  {}", s.line());
    }
    if let Some(msg) = report.blocking_message() {
        bail!("{msg}");
    }
    println!("\nAll required dependencies are satisfied.");
    Ok(())
}

fn install(args: InstallArgs, env: &DepsEnv) -> anyhow::Result<()> {
    let (blueprint, dir) = resolve_agent(&args.agent, env)?;
    // Load the whole config once, so an MCP-server install mutates and saves
    // this copy rather than re-reading the file.
    let mut config = load_config_or_default(&env.config_path)?;
    let report = dependencies::evaluate(
        &blueprint.dependencies,
        &config.mcp_servers,
        &dir,
        env.probe.as_ref(),
    );

    let mut acted = false;
    for (dep, status) in blueprint.dependencies.iter().zip(&report.statuses) {
        let satisfied = status.state.is_satisfied();
        if satisfied && !args.all {
            continue;
        }
        let Some(plan) = install_plan(dep, env.os) else {
            if !satisfied {
                println!(
                    "  {} ({}): nothing to install - {}",
                    dep.name,
                    dep.kind.tag(),
                    dep.remedy
                        .clone()
                        .unwrap_or_else(|| "no install steps declared".to_string())
                );
            }
            continue;
        };
        println!("\n{}", plan.summary(&dep.name));
        println!("  WARNING: this changes your machine (runs commands and/or writes config).");
        if !args.yes
            && !env
                .prompt
                .confirm(&format!("Install dependency '{}'?", dep.name))
        {
            println!("  skipped.");
            continue;
        }
        acted = true;
        run_plan(&plan, &dir, env, &mut config)?;
    }

    if !acted {
        println!("\nNo install steps were run.");
    } else {
        println!("\nRe-checking...");
    }
    // Always re-check, so the exit code reflects whether required dependencies
    // are now satisfied - a declined or partial install still reports as unmet.
    check(&args.agent, env)
}

/// What installing one dependency will do, resolved for this host's OS.
struct Plan {
    /// An MCP server to write into the config, and the secrets it still needs.
    server: Option<(MCPServerConfig, Vec<String>)>,
    /// A shell command to run.
    command: Option<String>,
    /// A Rhai install script (path relative to the blueprint).
    script: Option<String>,
}

impl Plan {
    fn summary(&self, name: &str) -> String {
        let mut lines = vec![format!("Install '{name}':")];
        if let Some((server, _)) = &self.server {
            lines.push(format!(
                "  - add MCP server '{}' to your config",
                server.name
            ));
        }
        if let Some(cmd) = &self.command {
            lines.push(format!("  - run: {cmd}"));
        }
        if let Some(script) = &self.script {
            lines.push(format!("  - run install script {script}"));
        }
        lines.join("\n")
    }
}

/// Build the install plan for a dependency, or `None` when it declares no way to
/// install itself.
fn install_plan(dep: &Dependency, os: &str) -> Option<Plan> {
    let install = dep.install.as_ref();
    let server = match &dep.kind {
        DependencyKind::McpServer { server, env } => install
            .and_then(|i| i.server.as_ref())
            .map(|tpl| (mcp_from_template(server, tpl), env.clone())),
        _ => None,
    };
    let command = install.and_then(|i| chosen_command(i, os));
    let script = install.and_then(|i| i.script.clone());
    if server.is_none() && command.is_none() && script.is_none() {
        return None;
    }
    Some(Plan {
        server,
        command,
        script,
    })
}

/// The install command for this OS: the per-OS entry, else the generic one.
fn chosen_command(install: &DependencyInstall, os: &str) -> Option<String> {
    install
        .commands
        .get(os)
        .cloned()
        .or_else(|| install.command.clone())
}

/// Turn a blueprint's MCP server template into a config entry.
fn mcp_from_template(
    name: &str,
    tpl: &leviath_core::blueprint::McpServerTemplate,
) -> MCPServerConfig {
    MCPServerConfig {
        name: name.to_string(),
        transport: tpl.transport.as_deref().map(|t| match t {
            "http" => MCPTransport::Http,
            // Validation restricts this to "stdio"/"http"; anything else would
            // have been refused at load, so stdio is the only other case.
            _ => MCPTransport::Stdio,
        }),
        command: tpl.command.clone(),
        url: tpl.url.clone(),
        args: tpl.args.clone(),
        env: std::collections::HashMap::new(),
        headers: tpl.headers.clone().into_iter().collect(),
    }
}

/// Carry out an install plan: write the server, run the command, run the script.
fn run_plan(
    plan: &Plan,
    dir: &Path,
    env: &DepsEnv,
    config: &mut crate::config::Config,
) -> anyhow::Result<()> {
    if let Some((server, secrets)) = &plan.server {
        add_server(config, server, &env.config_path)?;
        for var in secrets {
            if env
                .probe
                .env(var)
                .filter(|v| !v.trim().is_empty())
                .is_some()
            {
                println!("  {var} is already set.");
            } else {
                println!(
                    "  {var} is not set. It is a secret, so set it in your environment yourself,\n\
                     e.g. add `export {var}=...` to your shell profile, then open a new shell.\n\
                     It is read at connect time and never written to a file by this command."
                );
            }
        }
    }
    if let Some(cmd) = &plan.command {
        println!("  running: {cmd}");
        env.runner
            .run(cmd)
            .map_err(|e| anyhow::anyhow!("install command failed: {e}"))?;
        println!("  done.");
    }
    if let Some(script) = &plan.script {
        run_install_script(&dir.join(script), script, env)?;
        println!("  install script done.");
    }
    Ok(())
}

/// Add an MCP server to the config, unless one of that name is present, and save.
fn add_server(
    config: &mut crate::config::Config,
    server: &MCPServerConfig,
    path: &Path,
) -> anyhow::Result<()> {
    server
        .clone()
        .resolve()
        .map_err(|e| anyhow::anyhow!("the server template is not valid: {e}"))?;
    if config.mcp_servers.iter().any(|s| s.name == server.name) {
        println!("  MCP server '{}' is already configured.", server.name);
        return Ok(());
    }
    config.mcp_servers.push(server.clone());
    config.save_to_path_public(path)?;
    println!("  added MCP server '{}' to your config.", server.name);
    Ok(())
}

/// Run a Rhai install script, giving it one host function - `sh(command)` -
/// backed by the injected runner, plus the read-only probes a check gets.
fn run_install_script(path: &Path, shown: &str, env: &DepsEnv) -> anyhow::Result<()> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read install script {shown}"))?;
    let mut engine = rhai::Engine::new();
    leviath_scripting::harden(&mut engine, 1_000_000);
    let runner = env.runner.clone();
    engine.register_fn(
        "sh",
        move |cmd: &str| -> Result<(), Box<rhai::EvalAltResult>> {
            runner.run(cmd).map_err(|e| e.into())
        },
    );
    let ast = engine
        .compile(&source)
        .map_err(|e| anyhow::anyhow!("{shown}: {e}"))?;
    engine
        .call_fn::<()>(&mut rhai::Scope::new(), &ast, "install", ())
        .map_err(|e| anyhow::anyhow!("{shown}: install: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeProbe {
        env: HashMap<String, String>,
    }
    impl Probe for FakeProbe {
        fn env(&self, name: &str) -> Option<String> {
            self.env.get(name).cloned()
        }
        fn which(&self, _name: &str) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        ran: Mutex<Vec<String>>,
        fail: bool,
    }
    impl CommandRunner for FakeRunner {
        fn run(&self, command: &str) -> Result<(), String> {
            self.ran.lock().unwrap().push(command.to_string());
            if self.fail {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        }
    }

    struct FakePrompt {
        answer: bool,
    }
    impl Prompt for FakePrompt {
        fn confirm(&self, _message: &str) -> bool {
            self.answer
        }
    }

    fn env_with(
        dir: &Path,
        probe_env: HashMap<String, String>,
        runner: Arc<dyn CommandRunner>,
        confirm: bool,
    ) -> DepsEnv {
        DepsEnv {
            config_path: dir.join("config.toml"),
            agents_dir: Some(dir.join("agents")),
            probe: Box::new(FakeProbe { env: probe_env }),
            runner,
            prompt: Box::new(FakePrompt { answer: confirm }),
            os: "linux",
        }
    }

    fn write_agent(dir: &Path, name: &str, deps_toml: &str) -> PathBuf {
        let adir = dir.join("agents").join(name);
        std::fs::create_dir_all(&adir).unwrap();
        let manifest = adir.join("agent.leviath");
        std::fs::write(
            &manifest,
            format!(
                "[agent]\nname = \"{name}\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
                 [stages.main]\nmodel = {{ provider = \"anthropic\", model = \"m\" }}\n\n{deps_toml}"
            ),
        )
        .unwrap();
        adir
    }

    #[test]
    fn execute_with_routes_each_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"e\"\nkind = \"env\"\nvar = \"TOKEN\"\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::from([("TOKEN".into(), "x".into())]),
            Arc::new(FakeRunner::default()),
            true,
        );
        execute_with(
            DepsArgs {
                command: DepsCommand::List(AgentRef { agent: "a".into() }),
            },
            &env,
        )
        .unwrap();
        execute_with(
            DepsArgs {
                command: DepsCommand::Check(AgentRef { agent: "a".into() }),
            },
            &env,
        )
        .unwrap();
        execute_with(
            DepsArgs {
                command: DepsCommand::Install(InstallArgs {
                    agent: "a".into(),
                    yes: true,
                    all: false,
                }),
            },
            &env,
        )
        .unwrap();
    }

    #[test]
    fn host_os_is_one_of_the_known_keys() {
        assert!(["macos", "linux", "windows"].contains(&host_os()));
    }

    #[test]
    fn resolve_by_name_dir_file_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(dir.path(), "a", "");
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        // By installed name.
        assert!(resolve_agent("a", &env).is_ok());
        // By directory path.
        let adir = dir.path().join("agents").join("a");
        assert!(resolve_agent(adir.to_str().unwrap(), &env).is_ok());
        // By manifest path.
        let manifest = adir.join("agent.leviath");
        assert!(resolve_agent(manifest.to_str().unwrap(), &env).is_ok());
        // Missing.
        let err = resolve_agent("nope", &env).unwrap_err().to_string();
        assert!(err.contains("could not find agent 'nope'"), "{err}");
    }

    #[test]
    fn resolve_reports_parse_and_validate_errors() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        // Unparseable TOML.
        let bad = dir.path().join("agents").join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("agent.leviath"), "not = = valid").unwrap();
        assert!(
            resolve_agent("bad", &env)
                .unwrap_err()
                .to_string()
                .contains("parse")
        );
        // Parses but fails blueprint validation (two deps share a name).
        let inv = dir.path().join("agents").join("inv");
        std::fs::create_dir_all(&inv).unwrap();
        std::fs::write(
            inv.join("agent.leviath"),
            "[agent]\nname = \"inv\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\n\
             [stages.main]\nmodel = { provider = \"a\", model = \"m\" }\n\n\
             [[dependencies]]\nname = \"d\"\nkind = \"env\"\nvar = \"X\"\n\n\
             [[dependencies]]\nname = \"d\"\nkind = \"env\"\nvar = \"Y\"\n",
        )
        .unwrap();
        assert!(
            resolve_agent("inv", &env)
                .unwrap_err()
                .to_string()
                .contains("invalid blueprint")
        );
    }

    #[test]
    fn resolve_without_agents_dir_needs_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        env.agents_dir = None;
        let err = resolve_agent("bare-name", &env).unwrap_err().to_string();
        assert!(err.contains("no such path"), "{err}");
    }

    #[test]
    fn list_shows_declared_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"meshy\"\nkind = \"mcp_server\"\nserver = \"meshy\"\n\
             env = [\"MESHY_API_KEY\"]\ndescription = \"3d\"\n[dependencies.install.server]\nurl = \"https://x\"\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        list("a", &env).unwrap();
        // And an agent with none.
        write_agent(dir.path(), "b", "");
        list("b", &env).unwrap();
    }

    #[test]
    fn check_reports_satisfied_and_blocks_on_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"key\"\nkind = \"env\"\nvar = \"TOKEN\"\n",
        );
        // Missing -> error (non-zero exit).
        let miss = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        let err = check("a", &miss).unwrap_err().to_string();
        assert!(err.contains("not satisfied"), "{err}");
        // Present -> ok.
        let have = env_with(
            dir.path(),
            HashMap::from([("TOKEN".into(), "x".into())]),
            Arc::new(FakeRunner::default()),
            true,
        );
        check("a", &have).unwrap();
        // No deps -> ok.
        write_agent(dir.path(), "b", "");
        check("b", &have).unwrap();
    }

    #[test]
    fn install_writes_mcp_server_and_guides_for_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"meshy\"\nkind = \"mcp_server\"\nserver = \"meshy\"\n\
             env = [\"MESHY_API_KEY\"]\n[dependencies.install.server]\ntransport = \"http\"\nurl = \"https://api.meshy.ai/mcp\"\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env,
        )
        .unwrap_err(); // still blocking because the secret is not set
        // The server was written to config.
        let config = crate::config::Config::load_from_path_public(&env.config_path).unwrap();
        assert!(config.mcp_servers.iter().any(|s| s.name == "meshy"));
        // With the secret set and --all, re-installing is idempotent: the
        // server is already configured and the secret is already set.
        let env2 = env_with(
            dir.path(),
            HashMap::from([("MESHY_API_KEY".into(), "sk".into())]),
            Arc::new(FakeRunner::default()),
            true,
        );
        install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: true,
            },
            &env2,
        )
        .unwrap(); // now satisfied
    }

    #[test]
    fn install_runs_a_command_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"blender\"\nkind = \"binary\"\ncommand = \"blender\"\n\
             [dependencies.install]\ncommand = \"echo installing\"\n",
        );
        let runner = Arc::new(FakeRunner::default());
        let env = env_with(dir.path(), HashMap::new(), runner.clone(), true);
        // Binary is never found by FakeProbe, so it is unmet and the command runs.
        install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env,
        )
        .unwrap_err(); // still unmet after echo (fake which is always false)
        assert_eq!(runner.ran.lock().unwrap().as_slice(), ["echo installing"]);
        // A failing command surfaces the error.
        let failing = Arc::new(FakeRunner {
            fail: true,
            ..Default::default()
        });
        let env2 = env_with(dir.path(), HashMap::new(), failing, true);
        let err = install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: true,
            },
            &env2,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("install command failed"), "{err}");
    }

    #[test]
    fn install_declines_and_skips_when_not_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"b\"\nkind = \"binary\"\ncommand = \"x\"\n\
             [dependencies.install]\ncommand = \"echo hi\"\n",
        );
        let runner = Arc::new(FakeRunner::default());
        let env = env_with(dir.path(), HashMap::new(), runner.clone(), false);
        install(
            InstallArgs {
                agent: "a".into(),
                yes: false,
                all: false,
            },
            &env,
        )
        .unwrap_err();
        assert!(
            runner.ran.lock().unwrap().is_empty(),
            "declined install must not run"
        );
    }

    #[test]
    fn install_reports_when_nothing_to_do_or_no_steps() {
        let dir = tempfile::tempdir().unwrap();
        // Two dependencies with no install info: one bare (default message),
        // one carrying its own remedy.
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"key\"\nkind = \"env\"\nvar = \"TOKEN\"\n\n\
             [[dependencies]]\nname = \"key2\"\nkind = \"env\"\nvar = \"TOKEN2\"\n\
             remedy = \"ask an admin for TOKEN2\"\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        // Unmet, but nothing to install -> re-check still reports it as unmet.
        install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env,
        )
        .unwrap_err();
        // Everything satisfied -> re-check passes.
        let have = env_with(
            dir.path(),
            HashMap::from([("TOKEN".into(), "x".into()), ("TOKEN2".into(), "y".into())]),
            Arc::new(FakeRunner::default()),
            true,
        );
        install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &have,
        )
        .unwrap();
    }

    #[test]
    fn install_runs_a_rhai_script_with_sh() {
        let dir = tempfile::tempdir().unwrap();
        let adir = write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"thing\"\nkind = \"binary\"\ncommand = \"x\"\n\
             [dependencies.install]\nscript = \"install.rhai\"\n",
        );
        std::fs::write(
            adir.join("install.rhai"),
            r#"fn install() { sh("do it"); }"#,
        )
        .unwrap();
        let runner = Arc::new(FakeRunner::default());
        let env = env_with(dir.path(), HashMap::new(), runner.clone(), true);
        install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env,
        )
        .unwrap_err(); // binary still not found
        assert_eq!(runner.ran.lock().unwrap().as_slice(), ["do it"]);

        // A script whose sh() fails surfaces the error.
        std::fs::write(adir.join("install.rhai"), r#"fn install() { sh("nope"); }"#).unwrap();
        let failing = Arc::new(FakeRunner {
            fail: true,
            ..Default::default()
        });
        let env2 = env_with(dir.path(), HashMap::new(), failing, true);
        let err = install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env2,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("boom"), "{err}");

        // A script that will not compile surfaces a compile error naming it.
        std::fs::write(adir.join("install.rhai"), "fn install() {").unwrap();
        let env3 = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        let err = install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env3,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("install.rhai"), "{err}");
    }

    #[test]
    fn list_covers_every_dependency_kind() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "all",
            "[[dependencies]]\nname = \"m1\"\nkind = \"mcp_server\"\nserver = \"meshy\"\n\
             env = [\"KEY\"]\ndescription = \"d1\"\n[dependencies.install.server]\nurl = \"https://x\"\n\n\
             [[dependencies]]\nname = \"m2\"\nkind = \"mcp_server\"\nserver = \"other\"\n\n\
             [[dependencies]]\nname = \"e\"\nkind = \"env\"\nvar = \"TOKEN\"\n\n\
             [[dependencies]]\nname = \"b\"\nkind = \"binary\"\ncommand = \"blender\"\n\n\
             [[dependencies]]\nname = \"s\"\nkind = \"script\"\ncheck = \"chk.rhai\"\nrequired = false\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        list("all", &env).unwrap();
    }

    #[test]
    fn check_covers_every_output_arm() {
        let dir = tempfile::tempdir().unwrap();
        let adir = write_agent(
            dir.path(),
            "c",
            "[[dependencies]]\nname = \"ok\"\nkind = \"env\"\nvar = \"TOKEN\"\n\n\
             [[dependencies]]\nname = \"miss\"\nkind = \"binary\"\ncommand = \"nope\"\n\n\
             [[dependencies]]\nname = \"broke\"\nkind = \"script\"\ncheck = \"chk.rhai\"\n\n\
             [[dependencies]]\nname = \"opt\"\nkind = \"env\"\nvar = \"NOPE\"\nrequired = false\n",
        );
        std::fs::write(adir.join("chk.rhai"), r#"fn check() { throw "boom" }"#).unwrap();
        let env = env_with(
            dir.path(),
            HashMap::from([("TOKEN".into(), "x".into())]),
            Arc::new(FakeRunner::default()),
            true,
        );
        // ok=Satisfied, miss=Unmet, broke=Unusable, opt=optional-unmet -> blocks.
        check("c", &env).unwrap_err();
    }

    #[test]
    fn install_reports_an_invalid_server_template() {
        let dir = tempfile::tempdir().unwrap();
        // A server template with neither command nor url cannot resolve.
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"m\"\nkind = \"mcp_server\"\nserver = \"m\"\n\
             [dependencies.install.server]\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        let err = install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not valid"), "{err}");
    }

    #[test]
    fn a_bad_agent_errors_through_every_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        assert!(list("nope", &env).is_err());
        assert!(check("nope", &env).is_err());
        assert!(
            install(
                InstallArgs {
                    agent: "nope".into(),
                    yes: true,
                    all: false,
                },
                &env,
            )
            .is_err()
        );
    }

    #[test]
    fn a_corrupt_config_surfaces_through_check_and_install() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"m\"\nkind = \"mcp_server\"\nserver = \"m\"\n",
        );
        std::fs::write(dir.path().join("config.toml"), "this = = broken").unwrap();
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        assert!(check("a", &env).is_err());
        assert!(
            install(
                InstallArgs {
                    agent: "a".into(),
                    yes: true,
                    all: false,
                },
                &env,
            )
            .is_err()
        );
    }

    #[test]
    fn add_server_save_failure_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"m\"\nkind = \"mcp_server\"\nserver = \"m\"\n\
             [dependencies.install.server]\nurl = \"https://x\"\n",
        );
        // A file where the config's parent dir would go makes the save fail.
        std::fs::write(dir.path().join("blocker"), b"x").unwrap();
        let mut env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        env.config_path = dir.path().join("blocker").join("config.toml");
        assert!(
            install(
                InstallArgs {
                    agent: "a".into(),
                    yes: true,
                    all: false,
                },
                &env,
            )
            .is_err()
        );
    }

    #[test]
    fn a_missing_install_script_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"b\"\nkind = \"binary\"\ncommand = \"x\"\n\
             [dependencies.install]\nscript = \"missing.rhai\"\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::new(),
            Arc::new(FakeRunner::default()),
            true,
        );
        let err = install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: false,
            },
            &env,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("read install script"), "{err}");
    }

    #[test]
    fn install_all_skips_a_satisfied_dependency_with_no_steps() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "[[dependencies]]\nname = \"e\"\nkind = \"env\"\nvar = \"TOKEN\"\n",
        );
        let env = env_with(
            dir.path(),
            HashMap::from([("TOKEN".into(), "x".into())]),
            Arc::new(FakeRunner::default()),
            true,
        );
        // --all reaches the satisfied dep, which has no install steps: skipped.
        install(
            InstallArgs {
                agent: "a".into(),
                yes: true,
                all: true,
            },
            &env,
        )
        .unwrap();
    }

    #[test]
    fn chosen_command_prefers_the_per_os_entry() {
        let mut install = DependencyInstall {
            command: Some("generic".into()),
            ..Default::default()
        };
        assert_eq!(
            chosen_command(&install, "linux").as_deref(),
            Some("generic")
        );
        install.commands.insert("linux".into(), "apt".into());
        assert_eq!(chosen_command(&install, "linux").as_deref(), Some("apt"));
        assert_eq!(
            chosen_command(&install, "macos").as_deref(),
            Some("generic")
        );
    }

    #[test]
    fn mcp_template_maps_transport_and_headers() {
        let tpl = leviath_core::blueprint::McpServerTemplate {
            transport: Some("http".into()),
            url: Some("https://x".into()),
            headers: std::collections::BTreeMap::from([("A".into(), "b".into())]),
            ..Default::default()
        };
        let server = mcp_from_template("s", &tpl);
        assert_eq!(server.transport, Some(MCPTransport::Http));
        assert_eq!(server.headers.get("A").map(String::as_str), Some("b"));
        let stdio = leviath_core::blueprint::McpServerTemplate {
            transport: Some("stdio".into()),
            command: Some("srv".into()),
            ..Default::default()
        };
        assert_eq!(
            mcp_from_template("s", &stdio).transport,
            Some(MCPTransport::Stdio)
        );
    }
}
