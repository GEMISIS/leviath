//! Leviath CLI - `lev` command-line interface.

use clap::{Parser, Subcommand};
use tracing::info;

mod commands;
mod config;
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
    /// Initialize a new agent project
    Init(commands::init::InitArgs),
    
    /// Run an agent
    Run(commands::run::RunArgs),
    
    /// Spawn an agent from a blueprint
    Spawn(commands::spawn::SpawnArgs),
    
    /// List available agents and blueprints
    List(commands::list::ListArgs),
    
    /// Install an agent package
    Install(commands::install::InstallArgs),
    
    /// Run agent tests
    Test(commands::test::TestArgs),
    
    /// Inspect and debug context windows
    Context(commands::context::ContextArgs),

    /// Bundle an agent project for distribution
    Pack(commands::pack::PackArgs),

    /// Interactive agent dashboard
    Dashboard(commands::dashboard::DashboardArgs),

    /// List background agent runs
    Ps(commands::ps::PsArgs),

    /// Stream output from a background run
    Logs(commands::logs::LogsArgs),

    /// Stop a background run
    Stop(commands::stop::StopArgs),

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
        Commands::Init(args) => commands::init::execute(args).await,
        Commands::Run(args) => commands::run::execute(args).await,
        Commands::Spawn(args) => commands::spawn::execute(args).await,
        Commands::List(args) => commands::list::execute(args).await,
        Commands::Install(args) => commands::install::execute(args).await,
        Commands::Test(args) => commands::test::execute(args).await,
        Commands::Context(args) => commands::context::execute(args).await,
        Commands::Pack(args) => commands::pack::execute(args).await,
        Commands::Dashboard(args) => commands::dashboard::execute(args).await,
        Commands::Ps(args) => commands::ps::execute(args).await,
        Commands::Logs(args) => commands::logs::execute(args).await,
        Commands::Stop(args) => commands::stop::execute(args).await,
        Commands::RunWorker(args) => commands::run::execute_worker(args).await,
    }
}
