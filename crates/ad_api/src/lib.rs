//! AgentDialer HTTP API (axum).
//!
//! Design: a pure `router()` factory returns the full `axum::Router` so it can
//! be exercised in-process by tests via `tower::ServiceExt::oneshot` — no
//! network, no ports, fast and deterministic. `serve()` is the thin production
//! wrapper the `agentdialer api` subcommand calls.

mod campaign;
mod compliance;
mod dispatch;
mod hopper;
mod lead;
mod store;
mod webhook;

use std::sync::Arc;

use axum::{routing::get, Json, Router};
use serde::Serialize;

use ad_core::time::{Clock, SystemClock};

pub use store::{CampaignStore, LeadStore};
pub use webhook::Webhooks;

/// Shared application state. The `Clock` is injected so time-dependent
/// endpoints (compliance) stay deterministic under test. The stores are the
/// persistence boundary: in-memory for tests, Postgres in production.
#[derive(Clone)]
pub struct AppState {
    pub clock: Arc<dyn Clock>,
    pub campaigns: CampaignStore,
    pub leads: LeadStore,
    /// The LiveKit webhook endpoint answers 503 without it.
    pub webhooks: Option<Webhooks>,
}

impl AppState {
    /// In-memory state with the given clock. Used by tests and by local runs
    /// that set no database.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            campaigns: CampaignStore::in_memory(),
            leads: LeadStore::in_memory(),
            webhooks: None,
        }
    }

    /// State with explicit stores (for example Postgres).
    pub fn with_stores(clock: Arc<dyn Clock>, campaigns: CampaignStore, leads: LeadStore) -> Self {
        Self {
            clock,
            campaigns,
            leads,
            webhooks: None,
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
        .merge(dispatch::routes())
        .merge(webhook::routes())
        .with_state(state)
}

/// The settings of `serve`. The binary reads them from the environment
/// (`ad_bin` `config.rs`).
pub struct ServeConfig {
    pub addr: String,
    /// Postgres. Without it the API uses in-memory stores.
    pub database_url: Option<String>,
    /// The LiveKit API key and secret that verify webhooks.
    pub livekit_keys: Option<(String, String)>,
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig")
            .field("addr", &self.addr)
            .field("postgres", &self.database_url.is_some())
            .field("webhooks", &self.livekit_keys.is_some())
            .finish()
    }
}

/// Production entrypoint: bind and serve. Uses Postgres when a database URL is
/// set (and runs migrations first), else an in-memory store. The LiveKit
/// webhook endpoint needs both Postgres and the LiveKit keys.
pub async fn serve(config: ServeConfig) -> anyhow::Result<()> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let state = match &config.database_url {
        Some(url) => {
            let db = ad_db::Db::connect_ready(url, None).await?;
            db.migrate().await?;
            tracing::info!("stores: postgres (migrations applied)");
            let mut state = AppState::with_stores(
                clock,
                CampaignStore::Pg(db.campaigns()),
                LeadStore::Pg(db.leads()),
            );
            if let Some((key, secret)) = &config.livekit_keys {
                state.webhooks = Some(Webhooks {
                    verifier: ad_gateway::WebhookVerifier::new(key, secret),
                    attempts: db.attempts(),
                });
            }
            state
        }
        None => {
            tracing::warn!("stores: in-memory (DATABASE_URL unset; data is not durable)");
            AppState::new(clock)
        }
    };
    if state.webhooks.is_none() {
        tracing::warn!("LiveKit webhooks are off: they need DATABASE_URL and the LiveKit keys");
    }

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&config.addr).await?;
    tracing::info!(addr = %config.addr, "api listening");
    axum::serve(listener, app).await?;
    Ok(())
}
