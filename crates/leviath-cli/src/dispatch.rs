//! Command dispatch: the `Commands` enum and the `dispatch()` function that
//! routes a parsed subcommand to its executor.
//!
//! This lives in the library crate (not `main.rs`) so its routing logic can be
//! unit-tested under `cargo llvm-cov`'s `--lib` scope. The subcommands whose
//! real execution performs I/O a unit test must never trigger - a real
//! terminal takeover (`dash`), blocking stdin (`setup` interactive,
//! foreground `run`), binding a real port (`serve`), spawning a detached
//! worker or running a real inference loop (`run` background / `__run-worker`) -
//! are routed through the [`RiskyExecutors`] trait rather than called
//! directly. That way:
//!
//! * unit tests drive `dispatch()`'s full routing match against a
//!   `#[cfg(test)]` mock (`MockRisky`) that touches nothing real, and
//! * the real implementations live in the (coverage-unmeasured) `lev` binary
//!   as `main.rs`'s `RealExecutors`, which simply wires real I/O into the
//!   library's already-tested command cores.
//!
//! Injection gives a "routing is tested, real I/O is never touched by a test"
//! guarantee without any coverage escape hatch in library code.

use crate::commands;

/// Every `lev` subcommand. Each variant's doc comment is what its own
/// `lev <command> --help` prints.
///
/// The variants are ordered by section - setup, blueprints, running agents,
/// inspecting runs, servers - matching the categorized top-level help in
/// [`COMMANDS_HELP`]. Clap cannot group subcommands under headings itself (its
/// `help_heading` is args-only), so the top-level `lev --help` renders that
/// categorized block instead of clap's flat list, via [`HELP_TEMPLATE`]. A
/// section is not a namespace: every command is still invoked as a flat
/// `lev <command>`, and a test holds every variant to a line in
/// `COMMANDS_HELP` so a new command cannot be added without categorizing it.
#[derive(clap::Subcommand)]
pub enum Commands {
    // ─── Setup and configuration ──────────────────────────────────────────────
    /// Configure API keys and defaults
    Setup(commands::setup::SetupArgs),

    /// Show configured providers and set their priority order
    Providers(commands::providers::ProvidersArgs),

    /// Check that provider wiring works, end to end
    #[command(long_about = commands::doctor::DOCTOR_LONG_ABOUT)]
    Doctor(commands::doctor::DoctorArgs),

    /// List and inspect available models
    Models(commands::models::ModelsArgs),

    /// Inspect and move the secrets Leviath holds
    Auth(commands::auth::AuthArgs),

    /// Manage MCP tool servers, or serve Leviath itself as one
    Mcp(commands::mcp::McpArgs),

    /// Show what runs without an approval prompt, and why
    Approvals(commands::approvals::ApprovalsArgs),

    /// Manage taint tracking policy rules
    Policy(commands::policy::PolicyArgs),

    /// Update Leviath, then everything that shipped with it
    #[command(long_about = commands::update::UPDATE_LONG_ABOUT)]
    Update(commands::update::UpdateArgs),

    /// Register Leviath as an MCP server in Claude Code, Grok, Codex, Gemini or Hermes
    Integrate(commands::integrate::IntegrateArgs),

    // ─── Blueprints ───────────────────────────────────────────────────────────
    /// Create a new agent blueprint
    Create(commands::create::CreateArgs),

    /// List available and installed blueprints
    List(commands::list::ListArgs),

    /// Install a blueprint
    Add(commands::add::AddArgs),

    /// Remove an installed blueprint
    Remove(commands::remove::RemoveArgs),

    /// Validate an agent blueprint
    Validate(commands::validate::ValidateArgs),

    /// Run blueprint tests
    Test(commands::test::TestArgs),

    /// Bundle a blueprint for distribution
    Pack(commands::pack::PackArgs),

    /// List and validate the global Rhai script tools
    Tools(commands::tools::ToolsArgs),

    // ─── Running agents ───────────────────────────────────────────────────────
    /// Run an agent
    Run(commands::run::RunArgs),

    /// List agent runs in the shared-world daemon
    #[command(long_about = commands::ps::PS_LONG_ABOUT)]
    Ps(commands::ps::PsArgs),

    /// Send a message to a running agent
    Msg(commands::ctl::MsgArgs),

    /// Cancel a running agent (alias: `kill`)
    #[command(alias = "kill")]
    Cancel(commands::ctl::CancelArgs),

    /// Pause a running agent (it finishes its in-flight step, then holds)
    Pause(commands::ctl::PauseArgs),

    /// Resume a paused agent
    Resume(commands::ctl::ResumeArgs),

    /// Answer a pending interaction (or list open ones with no request id)
    Respond(commands::ctl::RespondArgs),

    /// Interactive agent dashboard
    #[command(name = "dash")]
    Dashboard(commands::dashboard::DashboardArgs),

    // ─── Inspecting runs ──────────────────────────────────────────────────────
    /// Print what an agent handed back when a run finished
    Result(commands::result::ResultArgs),

    /// Show a run's context-window history (from its run.lvr archive)
    Context(commands::context::ContextArgs),

    /// Show a run's per-stage token ledger, where a staged agent's cost lives
    Stages(commands::stages::StagesArgs),

    /// Show where a run's wall-clock time went: model calls, tools, waiting on children
    Timeline(commands::timeline::TimelineArgs),

    // ─── Servers ──────────────────────────────────────────────────────────────
    /// Start the REST + WebSocket API server
    Serve(commands::serve::ServeArgs),

    /// Serve this agent over the Agent Client Protocol (JSON-RPC over stdio)
    #[command(name = "agent-client")]
    AgentClient(commands::agent_client::AgentClientArgs),

    /// Run the shared-world daemon in the foreground
    Daemon(commands::daemon::DaemonArgs),
}

/// The top-level `lev --help` layout. Renders the categorized [`COMMANDS_HELP`]
/// (through `{after-help}`) in place of clap's flat subcommand list, which is
/// why the template names `{options}` rather than the usual `{all-args}` - the
/// latter would print the flat `Commands:` section this replaces.
pub const HELP_TEMPLATE: &str = "\
{about-with-newline}
{usage-heading} {usage}
{after-help}

Options:
{options}";

/// The categorized top-level command list, shown in place of clap's flat one.
///
/// Hand-maintained because clap cannot group subcommands, and held to the enum
/// by a test: every [`Commands`] variant must appear here, so a
/// command cannot be added without giving it a section. The descriptions match
/// each variant's doc comment; the test checks the names, and a reader keeps
/// the text in step.
pub const COMMANDS_HELP: &str = "\
Setup and configuration:
  setup         Configure API keys and defaults
  providers     Show configured providers and set their priority order
  doctor        Check that provider wiring works, end to end
  models        List and inspect available models
  auth          Inspect and move the secrets Leviath holds
  mcp           Manage MCP tool servers, or serve Leviath itself as one
  approvals     Show what runs without an approval prompt, and why
  policy        Manage taint tracking policy rules
  update        Update Leviath, then everything that shipped with it
  integrate     Register Leviath as an MCP server in Claude Code, Grok, Codex, Gemini or Hermes

Blueprints:
  create        Create a new agent blueprint
  list          List available and installed blueprints
  add           Install a blueprint
  remove        Remove an installed blueprint
  validate      Validate an agent blueprint
  test          Run blueprint tests
  pack          Bundle a blueprint for distribution
  tools         List and validate the global Rhai script tools

Running agents:
  run           Run an agent
  ps            List agent runs in the shared-world daemon
  msg           Send a message to a running agent
  cancel        Cancel a running agent (alias: `kill`)
  pause         Pause a running agent (it finishes its in-flight step, then holds)
  resume        Resume a paused agent
  respond       Answer a pending interaction (or list open ones with no request id)
  dash          Interactive agent dashboard

Inspecting runs:
  result        Print what an agent handed back when a run finished
  context       Show a run's context-window history (from its run.lvr archive)
  stages        Show a run's per-stage token ledger, where a staged agent's cost lives
  timeline      Show where a run's wall-clock time went: model calls, tools, waiting on children

Servers:
  serve         Start the REST + WebSocket API server
  agent-client  Serve this agent over the Agent Client Protocol (JSON-RPC over stdio)
  daemon        Run the shared-world daemon in the foreground

Run 'lev <command> --help' for detail on any command, or 'lev help <command>'.";

/// The subset of commands whose real execution performs I/O that a unit test
/// must never trigger. `dispatch()` routes these through this trait so its
/// routing logic stays unit-testable with a mock; the real implementations are
/// supplied by the binary (`main.rs`'s `RealExecutors`).
///
/// `async fn` in a trait is fine here: `dispatch` takes `&impl RiskyExecutors`
/// (static dispatch, no `dyn`), so no boxing or `Send` bound is required.
pub trait RiskyExecutors {
    // Each method returns `impl Future` rather than being an `async fn`, so what
    // the future promises is stated rather than inferred.
    //
    // Deliberately **not** `+ Send`. These run on the CLI's single-threaded
    // entry path and hold non-`Send` state across awaits - the daemon-readiness
    // poll takes a `&mut dyn FnMut() -> bool`, and the TUI paths hold terminal
    // handles. Adding the bound does not compile, which is the useful answer:
    // an `async fn` here left that unsaid, and this says it.
    /// `lev run` - auto-starts the daemon (real process spawn) if needed and
    /// spawns the agent into the shared world over the control socket.
    fn run(
        &self,
        args: commands::run::RunArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev ps` - resolves the control-socket path and queries the daemon.
    fn ps(
        &self,
        args: commands::ps::PsArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev msg` - resolves the control-socket path and sends a message.
    fn msg(
        &self,
        args: commands::ctl::MsgArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev cancel` - resolves the control-socket path and cancels a run.
    fn cancel(
        &self,
        args: commands::ctl::CancelArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev pause` - resolves the control-socket path and pauses a run.
    fn pause(
        &self,
        args: commands::ctl::PauseArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev resume` - resolves the control-socket path and resumes a run.
    fn resume(
        &self,
        args: commands::ctl::ResumeArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev respond` - resolves the control-socket path and answers/lists interactions.
    fn respond(
        &self,
        args: commands::ctl::RespondArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev doctor` - makes real billed inference calls, and (unless
    /// `--no-daemon`) auto-starts the daemon and spawns a throwaway run.
    fn doctor(
        &self,
        args: commands::doctor::DoctorArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev setup` - interactive (blocking stdin) or `--non-interactive`.
    fn setup(
        &self,
        args: commands::setup::SetupArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev dash` - takes over the real terminal and blocks on real keyboard input.
    fn dashboard(
        &self,
        args: commands::dashboard::DashboardArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev serve` - binds a real port and serves indefinitely.
    fn serve(
        &self,
        args: commands::serve::ServeArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev agent-client` - takes over real stdin/stdout to speak the Agent
    /// Client Protocol against the shared-world daemon.
    fn agent_client(
        &self,
        args: commands::agent_client::AgentClientArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev daemon` - binds the control socket and serves the shared world.
    fn daemon(
        &self,
        args: commands::daemon::DaemonArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev mcp` - rewrites config, opens a browser for OAuth, touches the token store.
    fn mcp(
        &self,
        args: commands::mcp::McpArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
    /// `lev providers` - reads the config file and may rewrite `provider_order`.
    fn providers(
        &self,
        args: commands::providers::ProvidersArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;

    /// `lev auth` - reads the config file and may write the OS credential store.
    fn auth(
        &self,
        args: commands::auth::AuthArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;

    /// `lev update` - resolves the real executable, shells out to a package
    /// manager, blocks on stdin for each confirmation, and rewrites the config.
    fn update(
        &self,
        args: commands::update::UpdateArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;

    /// `lev integrate` - writes a host agent's config under the real home
    /// directory and may run the host's own CLI.
    fn integrate(
        &self,
        args: commands::integrate::IntegrateArgs,
    ) -> impl std::future::Future<Output = anyhow::Result<()>>;
}

/// Inject argv-prescanned dynamic `--<region>` seed flags into a parsed
/// `run` command. A no-op for every other subcommand. Kept here (a tested lib
/// seam) so the bin entrypoint's post-parse wiring stays branch-free.
pub fn apply_region_flags(
    command: &mut Commands,
    regions: std::collections::HashMap<String, String>,
) {
    if let Commands::Run(args) = command {
        args.regions = regions;
    }
}

/// Route a parsed subcommand to its executor. Safe commands are called
/// directly (and are exercised through `dispatch()` by the tests below); the
/// I/O-risky ones go through `ex` (see [`RiskyExecutors`]).
pub async fn dispatch(command: Commands, ex: &impl RiskyExecutors) -> anyhow::Result<()> {
    match command {
        Commands::Create(args) => commands::create::execute(args).await,
        Commands::Setup(args) => ex.setup(args).await,
        Commands::Run(args) => ex.run(args).await,
        Commands::Ps(args) => ex.ps(args).await,
        Commands::Msg(args) => ex.msg(args).await,
        Commands::Cancel(args) => ex.cancel(args).await,
        Commands::Pause(args) => ex.pause(args).await,
        Commands::Resume(args) => ex.resume(args).await,
        Commands::Respond(args) => ex.respond(args).await,
        Commands::Doctor(args) => ex.doctor(args).await,
        Commands::List(args) => commands::list::execute(args).await,
        Commands::Add(args) => commands::add::execute(args).await,
        Commands::Remove(args) => commands::remove::execute(args).await,
        Commands::Test(args) => commands::test::execute(args).await,
        Commands::Pack(args) => commands::pack::execute(args).await,
        Commands::Dashboard(args) => ex.dashboard(args).await,
        Commands::Models(args) => commands::models::execute(args).await,
        Commands::Validate(args) => commands::validate::execute(args).await,
        Commands::Tools(args) => commands::tools::execute(args).await,
        Commands::Approvals(args) => commands::approvals::execute(args).await,
        Commands::Policy(args) => commands::policy::execute(args).await,
        Commands::Serve(args) => ex.serve(args).await,
        Commands::AgentClient(args) => ex.agent_client(args).await,
        Commands::Daemon(args) => ex.daemon(args).await,
        Commands::Context(args) => commands::context::execute(args).await,
        Commands::Stages(args) => commands::stages::execute(args).await,
        Commands::Timeline(args) => commands::timeline::execute(args).await,
        Commands::Result(args) => commands::result::execute(args).await,
        Commands::Mcp(args) => ex.mcp(args).await,
        Commands::Providers(args) => ex.providers(args).await,
        Commands::Auth(args) => ex.auth(args).await,
        Commands::Update(args) => ex.update(args).await,
        Commands::Integrate(args) => ex.integrate(args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command clap knows about is categorized in [`COMMANDS_HELP`], so a
    /// new subcommand cannot be added without giving it a section - the whole
    /// point of a hand-maintained list is that it not silently fall behind the
    /// enum. `augment_subcommands` yields the enum's own variants, not clap's
    /// auto `help` (added only when a top-level command is built), so every name
    /// here has an indented, described line in the list.
    #[test]
    fn every_command_is_listed_in_the_categorized_help() {
        use clap::{Command, Subcommand};
        let augmented = Commands::augment_subcommands(Command::new("lev"));
        for sub in augmented.get_subcommands() {
            let name = sub.get_name();
            assert!(
                COMMANDS_HELP.contains(&format!("  {name} ")),
                "`{name}` is not listed in COMMANDS_HELP - add it to a section"
            );
        }
    }

    /// The template renders the categorized list, not clap's flat one: it must
    /// carry `{after-help}` (where the list goes) and not `{subcommands}` or
    /// `{all-args}` (which would print the flat section this replaces).
    #[test]
    fn the_help_template_replaces_the_flat_command_list() {
        assert!(HELP_TEMPLATE.contains("{after-help}"), "{HELP_TEMPLATE}");
        assert!(
            !HELP_TEMPLATE.contains("{subcommands}") && !HELP_TEMPLATE.contains("{all-args}"),
            "{HELP_TEMPLATE}"
        );
    }

    /// Test double for [`RiskyExecutors`]: every method is a no-op returning
    /// `Ok(())`, so `dispatch()`'s risky routing arms are exercised without
    /// touching a real terminal / stdin / port / subprocess.
    struct MockRisky;

    impl RiskyExecutors for MockRisky {
        async fn run(&self, _args: commands::run::RunArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn ps(&self, _args: commands::ps::PsArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn msg(&self, _args: commands::ctl::MsgArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn respond(&self, _args: commands::ctl::RespondArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn doctor(&self, _args: commands::doctor::DoctorArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn cancel(&self, _args: commands::ctl::CancelArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn pause(&self, _args: commands::ctl::PauseArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn resume(&self, _args: commands::ctl::ResumeArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn setup(&self, _args: commands::setup::SetupArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn dashboard(&self, _args: commands::dashboard::DashboardArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn serve(&self, _args: commands::serve::ServeArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn agent_client(
            &self,
            _args: commands::agent_client::AgentClientArgs,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn daemon(&self, _args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
            Ok(())
        }
        async fn auth(&self, _args: commands::auth::AuthArgs) -> anyhow::Result<()> {
            Ok(())
        }

        async fn mcp(&self, _args: commands::mcp::McpArgs) -> anyhow::Result<()> {
            Ok(())
        }

        async fn providers(&self, _args: commands::providers::ProvidersArgs) -> anyhow::Result<()> {
            Ok(())
        }

        async fn update(&self, _args: commands::update::UpdateArgs) -> anyhow::Result<()> {
            Ok(())
        }

        async fn integrate(&self, _args: commands::integrate::IntegrateArgs) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn create_args() -> commands::create::CreateArgs {
        commands::create::CreateArgs {
            name: "unused".to_string(),
            template: "default".to_string(),
        }
    }

    // ─── apply_region_flags ──────────────────────────────────────────────────

    #[test]
    fn apply_region_flags_populates_run_and_noops_other_commands() {
        let mut run = Commands::Run(commands::run::RunArgs::default());
        let flags = std::collections::HashMap::from([("criteria".to_string(), "safe".to_string())]);
        apply_region_flags(&mut run, flags);
        assert!(
            matches!(&run, Commands::Run(a) if a.regions.get("criteria").map(String::as_str) == Some("safe")),
            "region flag was injected into the Run args"
        );
        // A non-run command hits the no-op branch: it must not panic (and there
        // is nothing to inject). Asserting the variant here would leave an
        // always-false `matches!` arm uncovered, so the call itself is the check.
        let mut other = Commands::Ps(commands::ps::PsArgs::default());
        apply_region_flags(&mut other, std::collections::HashMap::new());
    }

    // ─── Risky variants: routed through the injected executor ────────────────

    #[tokio::test]
    async fn dispatch_run_variant_is_routed_through_the_executor() {
        let result = dispatch(Commands::Run(commands::run::RunArgs::default()), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_setup_variant_is_routed_through_the_executor() {
        let args = commands::setup::SetupArgs {
            non_interactive: true,
            no_verify: false,
            install_agents: false,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
            claude_code: None,
            claude_code_effort: None,
            codex: None,
        };
        let result = dispatch(Commands::Setup(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_dashboard_variant_is_routed_through_the_executor() {
        let args = commands::dashboard::DashboardArgs {};
        let result = dispatch(Commands::Dashboard(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_msg_variant_is_routed_through_the_executor() {
        let args = commands::ctl::MsgArgs {
            agent_id: "a".to_string(),
            content: "c".to_string(),
        };
        assert!(dispatch(Commands::Msg(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_respond_variant_is_routed_through_the_executor() {
        let args = commands::ctl::RespondArgs {
            request_id: None,
            value: None,
            choice: None,
            approve: false,
            deny: false,
            feedback: None,
            session: false,
            stage: false,
            json: false,
        };
        assert!(dispatch(Commands::Respond(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_doctor_variant_is_routed_through_the_executor() {
        // Routed, not called directly: `lev doctor` bills two inferences and
        // auto-starts a daemon, so a unit test must never reach the real one.
        let args = commands::doctor::DoctorArgs::default();
        assert!(dispatch(Commands::Doctor(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_cancel_variant_is_routed_through_the_executor() {
        let args = commands::ctl::CancelArgs {
            run_id: "r".to_string(),
            force: false,
        };
        assert!(dispatch(Commands::Cancel(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_pause_variant_is_routed_through_the_executor() {
        let args = commands::ctl::PauseArgs {
            run_id: "r".to_string(),
        };
        assert!(dispatch(Commands::Pause(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_resume_variant_is_routed_through_the_executor() {
        let args = commands::ctl::ResumeArgs {
            run_id: "r".to_string(),
        };
        assert!(dispatch(Commands::Resume(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_ps_variant_is_routed_through_the_executor() {
        let result = dispatch(Commands::Ps(commands::ps::PsArgs::default()), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_daemon_variant_is_routed_through_the_executor() {
        let args = commands::daemon::DaemonArgs {
            action: None,
            socket: None,
        };
        let result = dispatch(Commands::Daemon(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_auth_variant_is_routed_through_the_executor() {
        let args = commands::auth::AuthArgs::status_for_test();
        let result = dispatch(Commands::Auth(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_update_variant_is_routed_through_the_executor() {
        // Routed, not called directly: the real `lev update` shells out to a
        // package manager and blocks on stdin, so a unit test must never reach
        // it. Its own tests drive the command core against injected seams.
        let args = commands::update::UpdateArgs::default();
        let result = dispatch(Commands::Update(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_integrate_variant_is_routed_through_the_executor() {
        // Routed, not called directly: the real command writes under the real
        // home directory and may run the host's CLI, neither of which a unit
        // test may touch. Its own tests drive the core against a tempdir.
        let args = commands::integrate::IntegrateArgs::claude_code_for_test();
        let result = dispatch(Commands::Integrate(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_mcp_variant_is_routed_through_the_executor() {
        let args = commands::mcp::McpArgs::list_for_test();
        let result = dispatch(Commands::Mcp(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_mcp_serve_variant_is_routed_through_the_executor() {
        // `lev mcp serve` takes over real stdio and talks to the daemon, so it
        // rides the same risky arm as the rest of `lev mcp`; the binary tells
        // the two apart with `McpArgs::route`.
        let args = commands::mcp::McpArgs::serve_for_test();
        let result = dispatch(Commands::Mcp(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_providers_variant_is_routed_through_the_executor() {
        // Routed, not called directly: the real command reads and rewrites the
        // config file, so a unit test must never reach it against the real one.
        let args = commands::providers::ProvidersArgs::list_for_test();
        let result = dispatch(Commands::Providers(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_serve_variant_is_routed_through_the_executor() {
        let args = commands::serve::ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: None,
            token: Some("test-token".to_string()),
            allow_admin: false,
            workdir_root: None,
            no_remote_yolo: false,
            tls_cert: None,
            tls_key: None,
            no_remote_seed_commands: false,
            max_concurrent_requests: None,
            request_timeout_secs: None,
        };
        let result = dispatch(Commands::Serve(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_agent_client_variant_is_routed_through_the_executor() {
        let args = commands::agent_client::AgentClientArgs::default();
        let result = dispatch(Commands::AgentClient(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    // ─── Safe variants: called directly, driven through dispatch() ───────────

    #[tokio::test]
    async fn dispatch_create_variant_is_routed() {
        // An already-existing directory makes `create::execute` return a real,
        // harmless `Err` without touching anything outside a tempdir.
        let dir = tempfile::tempdir().unwrap();
        let args = commands::create::CreateArgs {
            name: dir.path().to_str().unwrap().to_string(),
            ..create_args()
        };
        let result = dispatch(Commands::Create(args), &MockRisky).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_list_variant_is_routed() {
        // Isolated: this reaches `Config::load()`, which reads process-wide
        // environment. Unisolated it races every `temp_env` test in the binary.
        crate::config::with_isolated_config_path_async("dispatch-list", |_fake_dir| async move {
            let args = commands::list::ListArgs {
                filter: commands::list::ListFilter::All,
                json: false,
            };
            let result = dispatch(Commands::List(args), &MockRisky).await;
            assert!(result.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_add_variant_is_routed() {
        let args = commands::add::AddArgs {
            package: "definitely-not-a-real-bundle-xyz.leviath-bundle".to_string(),
        };
        // `add` loads the real config to report the `[read_paths]` grant status
        // of what it installs, so it needs the same isolation every other
        // config-touching test takes.
        let result = crate::config::with_isolated_config_path_async("dispatch-add", |_| {
            dispatch(Commands::Add(args), &MockRisky)
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_remove_variant_is_routed() {
        let args = commands::remove::RemoveArgs {
            name: "definitely-not-an-installed-agent-xyz".to_string(),
        };
        let result = dispatch(Commands::Remove(args), &MockRisky).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_test_variant_is_routed() {
        let dir = tempfile::tempdir().unwrap();
        let args = commands::test::TestArgs {
            path: Some(dir.path().to_str().unwrap().to_string()),
            filter: None,
            dry_run: true,
        };
        let result = dispatch(Commands::Test(args), &MockRisky).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_pack_variant_is_routed() {
        let dir = tempfile::tempdir().unwrap();
        let args = commands::pack::PackArgs {
            path: Some(dir.path().to_str().unwrap().to_string()),
            output: None,
        };
        let result = dispatch(Commands::Pack(args), &MockRisky).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_models_variant_is_routed() {
        crate::config::with_isolated_config_path_async("dispatch-models", |_fake_dir| async move {
            let args = commands::models::ModelsArgs {
                command: commands::models::ModelsCommand::List(commands::models::ListArgs {
                    provider: None,
                    remote: false,
                    offline: false,
                    all: false,
                    json: false,
                }),
            };
            let result = dispatch(Commands::Models(args), &MockRisky).await;
            assert!(result.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_validate_variant_is_routed() {
        // `validate` loads the real config to answer "can this install reach
        // the providers this blueprint names", so it needs the same isolation
        // every other config-touching test takes.
        crate::config::with_isolated_config_path_async("dispatch-validate", |_| async {
            let dir = tempfile::tempdir().unwrap();
            let args = commands::validate::ValidateArgs {
                path: dir
                    .path()
                    .join("does-not-exist")
                    .to_str()
                    .unwrap()
                    .to_string(),
                deny_warnings: false,
                json: false,
                graph: false,
                width: 120,
            };
            let result = dispatch(Commands::Validate(args), &MockRisky).await;
            assert!(result.is_err());
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_tools_variant_is_routed() {
        // Point LEVIATH_HOME at a temp dir so the scan is hermetic; an empty
        // tools dir just lists nothing and returns Ok (routing is exercised).
        let home = tempfile::tempdir().unwrap();
        let result = temp_env::async_with_vars(
            [("LEVIATH_HOME", Some(home.path().to_str().unwrap()))],
            async {
                let args = commands::tools::ToolsArgs { json: false };
                dispatch(Commands::Tools(args), &MockRisky).await
            },
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_routes_timeline() {
        let result = dispatch(
            Commands::Timeline(commands::timeline::TimelineArgs {
                run_id: "no-such-run".to_string(),
                json: false,
                calls: false,
                tree: false,
            }),
            &MockRisky,
        )
        .await;
        assert!(result.is_err(), "no journal for a run that never ran");
    }

    #[tokio::test]
    async fn dispatch_routes_stages() {
        // A run id that does not exist: the point is that the arm is wired to
        // the command, not what the command finds.
        let result = dispatch(
            Commands::Stages(commands::stages::StagesArgs {
                run_id: "no-such-run".to_string(),
                json: false,
                regions: false,
                visits: false,
            }),
            &MockRisky,
        )
        .await;
        assert!(result.is_err(), "no ledger for a run that never ran");
    }

    #[tokio::test]
    async fn dispatch_context_variant_is_routed() {
        // A run with no archive → the command errors (routing is exercised).
        let args = commands::context::ContextArgs {
            run_id: "no-such-run-xyzzy".to_string(),
            json: false,
            full: false,
        };
        let result = dispatch(Commands::Context(args), &MockRisky).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_result_variant_is_routed() {
        // A run that is not there → the command errors, which is what shows the
        // routing reached it.
        let args = commands::result::ResultArgs {
            run_id: "no-such-run-xyzzy".to_string(),
            json: false,
            raw: false,
        };
        let result = dispatch(Commands::Result(args), &MockRisky).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_approvals_variant_is_routed() {
        // A temp home means an empty config, so the report is the shipped
        // defaults and nothing touches the user's own file.
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        let result = temp_env::async_with_vars(
            [
                ("LEVIATH_HOME", Some(home.path().to_str().unwrap())),
                ("LEVIATH_CONFIG_PATH", Some(config.to_str().unwrap())),
            ],
            async {
                // Both spellings: with an agent named, and without, which is
                // the form that reports only what every agent gets.
                let args = commands::approvals::ApprovalsArgs {
                    command: commands::approvals::ApprovalsCommand::Safe(
                        commands::approvals::SafeArgs {
                            agent: Some("coder".to_string()),
                            json: true,
                        },
                    ),
                };
                dispatch(Commands::Approvals(args), &MockRisky).await
            },
        )
        .await;
        assert!(result.is_ok());
    }

    /// A config that will not parse has to surface, not be reported as "these
    /// are your defaults" - the whole point of the command is telling the user
    /// what is actually in effect.
    #[tokio::test]
    async fn dispatch_approvals_surfaces_a_broken_config() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("config.toml");
        std::fs::write(&config, "this is not = = toml").unwrap();
        let result = temp_env::async_with_vars(
            [
                ("LEVIATH_HOME", Some(home.path().to_str().unwrap())),
                ("LEVIATH_CONFIG_PATH", Some(config.to_str().unwrap())),
            ],
            async {
                let args = commands::approvals::ApprovalsArgs {
                    command: commands::approvals::ApprovalsCommand::Safe(
                        commands::approvals::SafeArgs {
                            agent: None,
                            json: false,
                        },
                    ),
                };
                dispatch(Commands::Approvals(args), &MockRisky).await
            },
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_policy_list_variant_is_routed() {
        let args = commands::policy::PolicyArgs {
            command: commands::policy::PolicyCommand::List(commands::policy::PolicyListArgs {}),
        };
        let result = dispatch(Commands::Policy(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_policy_test_variant_is_routed() {
        let args = commands::policy::PolicyArgs {
            command: commands::policy::PolicyCommand::Test(commands::policy::PolicyTestArgs {
                tool: "shell".to_string(),
                target: None,
                taint: "public".to_string(),
            }),
        };
        let result = dispatch(Commands::Policy(args), &MockRisky).await;
        assert!(result.is_ok());
    }
}
