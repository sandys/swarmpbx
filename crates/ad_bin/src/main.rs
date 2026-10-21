//! AgentDialer single-binary entrypoint. One image, run as different roles via
//! subcommand (PLAN.md §5): `api`, `orchestrator` (singleton), `dispatch`
//! (N replicas), `migrate` (Job). Subcommands are stubs until their phases land.

use clap::{Parser, Subcommand};

mod config;
mod orchestrator;

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
        Command::Orchestrator => orchestrator::run(),
        Command::Dispatch => not_yet("dispatch"),
        Command::Migrate => run_migrate(),
    }
}

fn run_api() -> anyhow::Result<()> {
    let serve = config::api_config(config::process_env)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(ad_api::serve(serve))
}

fn run_migrate() -> anyhow::Result<()> {
    config::check_required("migrate", config::MIGRATE, config::process_env)?;
    let url = config::process_env("DATABASE_URL").unwrap_or_default();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // A Kubernetes Job retries a failed migrate too; this covers a slow start.
        let db = ad_db::Db::connect_ready(&url, Some(std::time::Duration::from_secs(120))).await?;
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
