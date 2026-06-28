//! Leviath CLI - `lev` command-line interface.

use clap::{Parser, Subcommand};
use tracing::info;

mod commands;
mod config;
mod interaction;
mod runstate;
mod tools;

/// Leviath CLI - Agent framework with structured context windows
#[derive(Parser)]
#[command(name = "lev")]
#[command(about = "Leviath agent framework CLI", long_about = None)]
#[command(version)]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new agent blueprint
    Create(commands::create::CreateArgs),

    /// Configure API keys and defaults
    Setup(commands::setup::SetupArgs),

    /// Run an agent
    Run(commands::run::RunArgs),

    /// List available and installed blueprints
    List(commands::list::ListArgs),

    /// Install a blueprint
    Install(commands::install::InstallArgs),

    /// Remove an installed blueprint
    Uninstall(commands::uninstall::UninstallArgs),

    /// Run blueprint tests
    Test(commands::test::TestArgs),

    /// Bundle a blueprint for distribution
    Pack(commands::pack::PackArgs),

    /// Interactive agent dashboard
    Dashboard(commands::dashboard::DashboardArgs),

    /// List and inspect available models
    Models(commands::models::ModelsArgs),

    /// (Internal) Background worker process — do not call directly
    #[command(name = "__run-worker", hide = true)]
    RunWorker(commands::run::WorkerArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(level)
        .init();

    info!("Leviath CLI v{}", env!("CARGO_PKG_VERSION"));

    // Dispatch commands
    match cli.command {
        Commands::Create(args) => commands::create::execute(args).await,
        Commands::Setup(args) => commands::setup::execute(args).await,
        Commands::Run(args) => commands::run::execute(args).await,
        Commands::List(args) => commands::list::execute(args).await,
        Commands::Install(args) => commands::install::execute(args).await,
        Commands::Uninstall(args) => commands::uninstall::execute(args).await,
        Commands::Test(args) => commands::test::execute(args).await,
        Commands::Pack(args) => commands::pack::execute(args).await,
        Commands::Dashboard(args) => commands::dashboard::execute(args).await,
        Commands::Models(args) => commands::models::execute(args).await,
        Commands::RunWorker(args) => commands::run::execute_worker(args).await,
    }
}
