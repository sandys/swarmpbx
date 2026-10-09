//! AgentDialer HTTP API (axum).
//!
//! Design: a pure `router()` factory returns the full `axum::Router` so it can
//! be exercised in-process by tests via `tower::ServiceExt::oneshot` — no
//! network, no ports, fast and deterministic. `serve()` is the thin production
//! wrapper the `agentdialer api` subcommand calls.

mod campaign;
mod compliance;
mod hopper;
mod lead;
mod store;

use std::sync::Arc;

use axum::{routing::get, Json, Router};
use serde::Serialize;

use ad_core::time::{Clock, SystemClock};

pub use store::{CampaignStore, LeadStore};

/// Shared application state. The `Clock` is injected so time-dependent
/// endpoints (compliance) stay deterministic under test. The stores are the
/// persistence boundary: in-memory for tests, Postgres in production.
#[derive(Clone)]
pub struct AppState {
    pub clock: Arc<dyn Clock>,
    pub campaigns: CampaignStore,
    pub leads: LeadStore,
}

impl AppState {
    /// In-memory state with the given clock. Used by tests and by local runs
    /// that set no database.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            campaigns: CampaignStore::in_memory(),
            leads: LeadStore::in_memory(),
        }
    }

    /// State with explicit stores (for example Postgres).
    pub fn with_stores(clock: Arc<dyn Clock>, campaigns: CampaignStore, leads: LeadStore) -> Self {
        Self {
            clock,
            campaigns,
            leads,
        }
    }
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
}

async fn health() -> Json<Health> {
    Json(Health {
        status: "ok",
        version: ad_core::version(),
    })
}

/// Build the full application router. This is the single source of truth for
/// routes and is what both `serve()` and the tests use.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(compliance::routes())
        .merge(campaign::routes())
        .merge(lead::routes())
        .merge(hopper::routes())
        .with_state(state)
}

/// Production entrypoint: bind and serve. Uses Postgres when `DATABASE_URL` is
/// set (and runs migrations first), else an in-memory store.
pub async fn serve(addr: &str) -> anyhow::Result<()> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (campaigns, leads) = match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => {
            let db = ad_db::Db::connect(&url).await?;
            db.migrate().await?;
            tracing::info!("stores: postgres (migrations applied)");
            (CampaignStore::Pg(db.campaigns()), LeadStore::Pg(db.leads()))
        }
        _ => {
            tracing::warn!("stores: in-memory (DATABASE_URL unset; data is not durable)");
            (CampaignStore::in_memory(), LeadStore::in_memory())
        }
    };

    let app = router(AppState::with_stores(clock, campaigns, leads));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "api listening");
    axum::serve(listener, app).await?;
    Ok(())
}
