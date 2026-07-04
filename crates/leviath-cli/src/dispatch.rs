//! Command dispatch: the `Commands` enum and the `dispatch()` function that
//! routes a parsed subcommand to its executor.
//!
//! This lives in the library crate (not `main.rs`, the binary's own entry
//! point) specifically so it can be unit-tested under `cargo llvm-cov`'s
//! `--lib` scope. `main.rs`'s own code is invisible to that scope entirely
//! -- `xtask/src/coverage.rs` only ever runs `--lib` plus one `--test
//! <name>` scope per integration-test file for a package, deliberately
//! never a `--bin` scope (see `package_test_targets`'s doc comment there) --
//! so a `#[cfg(test)] mod tests` placed directly in `main.rs` would compile
//! and pass under `cargo test --bin lev`, but would never actually be
//! executed by `cargo xtask coverage`, leaving its measured coverage
//! unchanged. `main.rs`'s only real coverage comes from
//! `tests/cli_dispatch.rs` spawning the actual compiled `lev` binary, which
//! deliberately never spawns `dash`/`run` (foreground)/`serve`/
//! `__run-worker` for hard safety reasons documented there (no test may
//! touch a real terminal/TTY, bind a real network port, or spawn a
//! subprocess that could hang).
//!
//! Moving the dispatch match here means its command-construction/
//! argument-passing logic can be driven directly by a real unit test (see
//! `tests` below) that constructs every `Commands` variant and calls
//! `dispatch()`, while the 3 risky underlying executors
//! (`commands::run::execute`, `commands::serve::execute`,
//! `commands::run::execute_worker`) are reached through
//! `#[cfg(not(test))]`/`#[cfg(test)]` twins so the unit test never actually
//! touches a real terminal/network/subprocess -- exactly what
//! `cli_dispatch.rs`'s own docs already promise never happens, just
//! verified by a different, still totally safe mechanism.
//! (`commands::dashboard::execute` already has its own such twin -- see
//! `commands/dashboard/mod.rs` -- so `Commands::Dashboard` is dispatched
//! straight through without a wrapper here.)

use crate::commands;

#[derive(clap::Subcommand)]
pub enum Commands {
    /// Create a new agent blueprint
    Create(commands::create::CreateArgs),

    /// Configure API keys and defaults
    Setup(commands::setup::SetupArgs),

    /// Run an agent
    Run(commands::run::RunArgs),

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

    /// Start the REST + WebSocket API server
    Serve(commands::serve::ServeArgs),

    /// (Internal) Background worker process — do not call directly
    #[command(name = "__run-worker", hide = true)]
    RunWorker(commands::run::WorkerArgs),
}

/// Route a parsed subcommand to its executor. Extracted from `main()` so it
/// can be unit-tested (see the module doc comment above for why that
/// requires living here rather than in `main.rs`).
pub async fn dispatch(command: Commands) -> anyhow::Result<()> {
    match command {
        Commands::Create(args) => commands::create::execute(args).await,
        Commands::Setup(args) => commands::setup::execute(args).await,
        Commands::Run(args) => dispatch_run(args).await,
        Commands::List(args) => commands::list::execute(args).await,
        Commands::Add(args) => commands::add::execute(args).await,
        Commands::Remove(args) => commands::remove::execute(args).await,
        Commands::Test(args) => commands::test::execute(args).await,
        Commands::Pack(args) => commands::pack::execute(args).await,
        Commands::Dashboard(args) => commands::dashboard::execute(args).await,
        Commands::Models(args) => commands::models::execute(args).await,
        Commands::Validate(args) => commands::validate::execute(args).await,
        Commands::Serve(args) => dispatch_serve(args).await,
        Commands::RunWorker(args) => dispatch_run_worker(args).await,
    }
}

/// COVERAGE-EXCLUDED: `commands::run::execute` can run in foreground mode
/// (reading real stdin / writing the real terminal) or spawn a real
/// detached background worker process -- neither of which any test in this
/// suite may safely do (see the hard safety rule in this module's doc
/// comment). `dispatch()`'s own routing logic is still fully exercised by
/// `dispatch_run_variant_is_routed`/etc. below; only the real execution is
/// swapped for a no-op under test.
#[cfg(not(test))]
async fn dispatch_run(args: commands::run::RunArgs) -> anyhow::Result<()> {
    commands::run::execute(args).await
}

#[cfg(test)]
async fn dispatch_run(_args: commands::run::RunArgs) -> anyhow::Result<()> {
    Ok(())
}

/// COVERAGE-EXCLUDED: see `dispatch_run` -- `commands::serve::execute`
/// binds a real network port and serves real HTTP/WebSocket traffic
/// indefinitely (until a shutdown signal `dispatch()`'s caller never
/// supplies), which no test in this suite may safely do.
#[cfg(not(test))]
async fn dispatch_serve(args: commands::serve::ServeArgs) -> anyhow::Result<()> {
    commands::serve::execute(args).await
}

#[cfg(test)]
async fn dispatch_serve(_args: commands::serve::ServeArgs) -> anyhow::Result<()> {
    Ok(())
}

/// COVERAGE-EXCLUDED: see `dispatch_run` -- `commands::run::execute_worker`
/// is the background worker entry point, spawned as its own real subprocess
/// by `commands::run::execute`'s background path; running it directly here
/// would perform real inference-loop/subprocess work, which no test in this
/// suite may safely do.
#[cfg(not(test))]
async fn dispatch_run_worker(args: commands::run::WorkerArgs) -> anyhow::Result<()> {
    commands::run::execute_worker(args).await
}

#[cfg(test)]
async fn dispatch_run_worker(_args: commands::run::WorkerArgs) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_args() -> commands::create::CreateArgs {
        commands::create::CreateArgs {
            name: "unused".to_string(),
            template: "software-engineer".to_string(),
        }
    }

    #[tokio::test]
    async fn dispatch_run_variant_is_routed_without_touching_real_environment() {
        let args = commands::run::RunArgs {
            path: None,
            task: None,
            model: None,
            foreground: false,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
            count: 1,
        };
        let result = dispatch(Commands::Run(args)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_serve_variant_is_routed_without_binding_a_real_port() {
        let args = commands::serve::ServeArgs {
            port: 0,
            host: "127.0.0.1".to_string(),
            cors: "*".to_string(),
        };
        let result = dispatch(Commands::Serve(args)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_run_worker_variant_is_routed_without_a_real_subprocess() {
        let args = commands::run::WorkerArgs {
            path: "/unused/path".to_string(),
            task: "unused task".to_string(),
            run_id: "unused-run-id".to_string(),
            model: None,
            yolo: false,
            allow: vec![],
            ask: vec![],
            deny: vec![],
            max_depth: None,
        };
        let result = dispatch(Commands::RunWorker(args)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_dashboard_variant_is_routed_via_its_own_test_twin() {
        // `commands::dashboard::execute` already has its own `#[cfg(test)]`
        // no-op twin (see `commands/dashboard/mod.rs`), so `dispatch()`
        // calls it directly with no wrapper of its own.
        let args = commands::dashboard::DashboardArgs {};
        let result = dispatch(Commands::Dashboard(args)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_create_variant_is_routed() {
        // A quick sanity check that non-risky variants still flow through
        // `dispatch()`'s match unchanged: an already-existing directory
        // makes `create::execute` return a real, harmless `Err` without
        // touching anything outside a tempdir.
        let dir = tempfile::tempdir().unwrap();
        let args = commands::create::CreateArgs {
            name: dir.path().to_str().unwrap().to_string(),
            ..create_args()
        };
        let result = dispatch(Commands::Create(args)).await;
        assert!(result.is_err());
    }

    // The remaining variants below don't need a real/fake distinction --
    // their executors are already safe to call directly -- but `dispatch()`
    // itself was never previously routed through for them, leaving those
    // specific match-arm regions uncovered even though the executors they
    // call are separately, thoroughly tested elsewhere. Each test below
    // just needs to prove the arm is reachable and passes its args through.

    #[tokio::test]
    async fn dispatch_setup_variant_is_routed() {
        // Isolated config path: `--non-interactive` setup saves a real
        // config file, which must not land in the developer's real
        // `~/.leviath/config.toml`.
        let guard = crate::config::isolate_config_path_for_test("dispatch-setup");
        let args = commands::setup::SetupArgs {
            non_interactive: true,
            anthropic_key: None,
            openai_key: None,
            google_key: None,
            openrouter_key: None,
            ollama_url: None,
            default_model: None,
        };
        let result = dispatch(Commands::Setup(args)).await;
        drop(guard);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_list_variant_is_routed() {
        let args = commands::list::ListArgs {
            filter: "all".to_string(),
        };
        let result = dispatch(Commands::List(args)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_add_variant_is_routed() {
        let args = commands::add::AddArgs {
            package: "definitely-not-a-real-bundle-xyz.leviath-bundle".to_string(),
            registry: None,
        };
        let result = dispatch(Commands::Add(args)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_remove_variant_is_routed() {
        let args = commands::remove::RemoveArgs {
            name: "definitely-not-an-installed-agent-xyz".to_string(),
        };
        let result = dispatch(Commands::Remove(args)).await;
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
        let result = dispatch(Commands::Test(args)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_pack_variant_is_routed() {
        let dir = tempfile::tempdir().unwrap();
        let args = commands::pack::PackArgs {
            path: Some(dir.path().to_str().unwrap().to_string()),
            output: None,
        };
        let result = dispatch(Commands::Pack(args)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn dispatch_models_variant_is_routed() {
        let guard = crate::config::isolate_config_path_for_test("dispatch-models");
        let args = commands::models::ModelsArgs {
            command: commands::models::ModelsCommand::List(commands::models::ListArgs {
                provider: None,
                remote: false,
            }),
        };
        let result = dispatch(Commands::Models(args)).await;
        drop(guard);
        assert!(result.is_ok());
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
        let result = dispatch(Commands::Validate(args)).await;
        assert!(result.is_err());
    }
}
