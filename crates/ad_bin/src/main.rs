//! AgentDialer single-binary entrypoint. One image, run as different roles via
//! subcommand (PLAN.md §5): `api`, `orchestrator` (singleton), `dispatch`
//! (N replicas), `migrate` (Job). Subcommands are stubs until their phases land.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agentdialer",
    version,
    about = "SIP-only, AI-agent-native dialer"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP/control API server.
    Api,
    /// Run the campaign orchestrator (singleton; leader-elected via pg advisory lock).
    Orchestrator,
    /// Run a dispatch worker (claims leads, gates on compliance, originates calls).
    Dispatch,
    /// Apply database migrations and exit.
    Migrate,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    tracing::info!(version = ad_core::version(), "agentdialer starting");

    match cli.command {
        Command::Api => run_api(),
        Command::Orchestrator => not_yet("orchestrator"),
        Command::Dispatch => not_yet("dispatch"),
        Command::Migrate => run_migrate(),
    }
}

fn run_api() -> anyhow::Result<()> {
    let addr = std::env::var("AD_API_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(ad_api::serve(&addr))
}

fn run_migrate() -> anyhow::Result<()> {
    let url = std::env::var("DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DATABASE_URL must be set to run migrations"))?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let db = ad_db::Db::connect(&url).await?;
        db.migrate().await?;
        tracing::info!("migrations applied");
        println!("agentdialer: migrations applied");
        Ok(())
    })
}

fn not_yet(role: &str) -> anyhow::Result<()> {
    tracing::warn!(role, "role not implemented yet (scaffold phase)");
    println!("agentdialer: role '{role}' is scaffolded but not implemented yet");
    Ok(())
}
