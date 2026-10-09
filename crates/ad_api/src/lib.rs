//! AgentDialer HTTP API (axum).
//!
//! Design: a pure `router()` factory returns the full `axum::Router` so it can
//! be exercised in-process by tests via `tower::ServiceExt::oneshot` — no
//! network, no ports, fast and deterministic. `serve()` is the thin production
//! wrapper the `agentdialer api` subcommand calls.

mod compliance;

use std::sync::Arc;

use axum::{routing::get, Json, Router};
use serde::Serialize;

use ad_core::time::{Clock, SystemClock};

/// Shared application state. The `Clock` is injected so time-dependent
/// endpoints (compliance) are deterministic under test.
#[derive(Clone)]
pub struct AppState {
    pub clock: Arc<dyn Clock>,
}

impl AppState {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }
    pub fn with_system_clock() -> Self {
        Self {
            clock: Arc::new(SystemClock),
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
        .with_state(state)
}

/// Production entrypoint: bind and serve the router.
pub async fn serve(addr: &str) -> anyhow::Result<()> {
    let state = AppState::with_system_clock();
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "api listening");
    axum::serve(listener, app).await?;
    Ok(())
}
