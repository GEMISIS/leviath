//! Leviath CLI - `lev` command-line interface.

use clap::{Parser, Subcommand};
use tracing::info;

mod commands;
mod config;
mod interaction;
mod render;
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

    /// Benchmark cache efficiency for an agent
    Bench(commands::bench::BenchArgs),

    /// Start the REST + WebSocket API server
    Serve(commands::serve::ServeArgs),

    /// (Internal) Background worker process — do not call directly
    #[command(name = "__run-worker", hide = true)]
    RunWorker(commands::run::WorkerArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    let level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt().with_env_filter(level).init();

    info!("Leviath CLI v{}", env!("CARGO_PKG_VERSION"));

    // Dispatch commands
    match cli.command {
        Commands::Create(args) => commands::create::execute(args).await,
        Commands::Setup(args) => commands::setup::execute(args).await,
        Commands::Run(args) => commands::run::execute(args).await,
        Commands::List(args) => commands::list::execute(args).await,
        Commands::Add(args) => commands::add::execute(args).await,
        Commands::Remove(args) => commands::remove::execute(args).await,
        Commands::Test(args) => commands::test::execute(args).await,
        Commands::Pack(args) => commands::pack::execute(args).await,
        Commands::Dashboard(args) => commands::dashboard::execute(args).await,
        Commands::Models(args) => commands::models::execute(args).await,
        Commands::Validate(args) => commands::validate::execute(args).await,
        Commands::Serve(args) => commands::serve::execute(args).await,
        Commands::Bench(args) => commands::bench::execute(args).await,
        Commands::RunWorker(args) => commands::run::execute_worker(args).await,
    }
}
