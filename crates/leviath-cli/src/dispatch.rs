//! Command dispatch: the `Commands` enum and the `dispatch()` function that
//! routes a parsed subcommand to its executor.
//!
//! This lives in the library crate (not `main.rs`) so its routing logic can be
//! unit-tested under `cargo llvm-cov`'s `--lib` scope. The subcommands whose
//! real execution performs I/O a unit test must never trigger — a real
//! terminal takeover (`dash`), blocking stdin (`setup` interactive,
//! foreground `run`), binding a real port (`serve`), spawning a detached
//! worker or running a real inference loop (`run` background / `__run-worker`)
//! — are routed through the [`RiskyExecutors`] trait rather than called
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

#[derive(clap::Subcommand)]
pub enum Commands {
    /// Create a new agent blueprint
    Create(commands::create::CreateArgs),

    /// Configure API keys and defaults
    Setup(commands::setup::SetupArgs),

    /// Run an agent
    Run(commands::run::RunArgs),

    /// List agents running in the shared-world daemon
    Ps(commands::ps::PsArgs),

    /// Send a message to a running agent
    Msg(commands::ctl::MsgArgs),

    /// Cancel a running agent
    Cancel(commands::ctl::CancelArgs),

    /// Answer a pending interaction (or list open ones with no request id)
    Respond(commands::ctl::RespondArgs),

    /// List available and installed blueprints
    List(commands::list::ListArgs),

    /// Install a blueprint
    Add(commands::add::AddArgs),

    /// Remove an installed blueprint
    Remove(commands::remove::RemoveArgs),

    /// Run blueprint tests
    Test(commands::test::TestArgs),

    /// Bundle a blueprint for distribution
    Pack(commands::pack::PackArgs),

    /// Interactive agent dashboard
    #[command(name = "dash")]
    Dashboard(commands::dashboard::DashboardArgs),

    /// List and inspect available models
    Models(commands::models::ModelsArgs),

    /// Validate an agent blueprint
    Validate(commands::validate::ValidateArgs),

    /// Manage taint tracking policy rules
    Policy(commands::policy::PolicyArgs),

    /// Start the REST + WebSocket API server
    Serve(commands::serve::ServeArgs),

    /// Run the shared-world daemon in the foreground
    Daemon(commands::daemon::DaemonArgs),
}

/// The subset of commands whose real execution performs I/O that a unit test
/// must never trigger. `dispatch()` routes these through this trait so its
/// routing logic stays unit-testable with a mock; the real implementations are
/// supplied by the binary (`main.rs`'s `RealExecutors`).
///
/// `async fn` in a trait is fine here: `dispatch` takes `&impl RiskyExecutors`
/// (static dispatch, no `dyn`), so no boxing or `Send` bound is required.
#[allow(async_fn_in_trait)]
pub trait RiskyExecutors {
    /// `lev run` — auto-starts the daemon (real process spawn) if needed and
    /// spawns the agent into the shared world over the control socket.
    async fn run(&self, args: commands::run::RunArgs) -> anyhow::Result<()>;
    /// `lev ps` — resolves the control-socket path and queries the daemon.
    async fn ps(&self, args: commands::ps::PsArgs) -> anyhow::Result<()>;
    /// `lev msg` — resolves the control-socket path and sends a message.
    async fn msg(&self, args: commands::ctl::MsgArgs) -> anyhow::Result<()>;
    /// `lev cancel` — resolves the control-socket path and cancels a run.
    async fn cancel(&self, args: commands::ctl::CancelArgs) -> anyhow::Result<()>;
    /// `lev respond` — resolves the control-socket path and answers/lists interactions.
    async fn respond(&self, args: commands::ctl::RespondArgs) -> anyhow::Result<()>;
    /// `lev setup` — interactive (blocking stdin) or `--non-interactive`.
    async fn setup(&self, args: commands::setup::SetupArgs) -> anyhow::Result<()>;
    /// `lev dash` — takes over the real terminal and blocks on real keyboard input.
    async fn dashboard(&self, args: commands::dashboard::DashboardArgs) -> anyhow::Result<()>;
    /// `lev serve` — binds a real port and serves indefinitely.
    async fn serve(&self, args: commands::serve::ServeArgs) -> anyhow::Result<()>;
    /// `lev daemon` — binds the control socket and serves the shared world.
    async fn daemon(&self, args: commands::daemon::DaemonArgs) -> anyhow::Result<()>;
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
        Commands::Respond(args) => ex.respond(args).await,
        Commands::List(args) => commands::list::execute(args).await,
        Commands::Add(args) => commands::add::execute(args).await,
        Commands::Remove(args) => commands::remove::execute(args).await,
        Commands::Test(args) => commands::test::execute(args).await,
        Commands::Pack(args) => commands::pack::execute(args).await,
        Commands::Dashboard(args) => ex.dashboard(args).await,
        Commands::Models(args) => commands::models::execute(args).await,
        Commands::Validate(args) => commands::validate::execute(args).await,
        Commands::Policy(args) => commands::policy::execute(args).await,
        Commands::Serve(args) => ex.serve(args).await,
        Commands::Daemon(args) => ex.daemon(args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        async fn cancel(&self, _args: commands::ctl::CancelArgs) -> anyhow::Result<()> {
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
        async fn daemon(&self, _args: commands::daemon::DaemonArgs) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn create_args() -> commands::create::CreateArgs {
        commands::create::CreateArgs {
            name: "unused".to_string(),
            template: "software-engineer".to_string(),
        }
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
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
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
            session: false,
        };
        assert!(dispatch(Commands::Respond(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_cancel_variant_is_routed_through_the_executor() {
        let args = commands::ctl::CancelArgs {
            run_id: "r".to_string(),
        };
        assert!(dispatch(Commands::Cancel(args), &MockRisky).await.is_ok());
    }

    #[tokio::test]
    async fn dispatch_ps_variant_is_routed_through_the_executor() {
        let result = dispatch(Commands::Ps(commands::ps::PsArgs {}), &MockRisky).await;
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
    async fn dispatch_serve_variant_is_routed_through_the_executor() {
        let args = commands::serve::ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };
        let result = dispatch(Commands::Serve(args), &MockRisky).await;
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
        let args = commands::list::ListArgs {
            filter: "all".to_string(),
        };
        let result = dispatch(Commands::List(args), &MockRisky).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_add_variant_is_routed() {
        let args = commands::add::AddArgs {
            package: "definitely-not-a-real-bundle-xyz.leviath-bundle".to_string(),
            registry: None,
        };
        let result = dispatch(Commands::Add(args), &MockRisky).await;
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
                }),
            };
            let result = dispatch(Commands::Models(args), &MockRisky).await;
            assert!(result.is_ok());
        })
        .await;
    }

    #[tokio::test]
    async fn dispatch_validate_variant_is_routed() {
        let dir = tempfile::tempdir().unwrap();
        let args = commands::validate::ValidateArgs {
            path: dir
                .path()
                .join("does-not-exist")
                .to_str()
                .unwrap()
                .to_string(),
        };
        let result = dispatch(Commands::Validate(args), &MockRisky).await;
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
