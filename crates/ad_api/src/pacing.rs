//! Pacing API: the join-paced window of a campaign's agent pool (GAPS.md
//! DIAL-9, decision D-10). Every value changes at run time; the leader's next
//! claim reads it.
//!
//! Endpoints (Postgres only; 503 without it):
//!   GET /v1/campaigns/:id/pacing  the campaign's mode and limit, and its pool
//!                                 (404 if missing; 409 without an agent, or
//!                                 when the saved settings do not read)
//!   PUT /v1/campaigns/:id/pacing  {"mode": "adaptive" | "strict",
//!                                  "pool": {"target_ms", "backoff_ms",
//!                                           "first_k", "max_k"},
//!                                  "reset": true}
//!                                 Every field is optional. `pool` replaces
//!                                 the settings of the pool, which every
//!                                 campaign of the pool shares (400 if
//!                                 invalid). `reset` starts the pool again at
//!                                 its first limit, in slow start.
//!   GET /v1/agent-pools           every pool; a pool whose settings do not
//!                                 read shows {"pool", "error"}
//!
//! The values are the pool's state at its last learning step: the leader
//! learns from new samples before each claim and on each sweep.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use ad_agent::AgentSpec;
use ad_campaign::repo::RepoError;
use ad_compliance::abandon::JurisdictionRule;
use ad_core::CampaignId;
use ad_db::{Listed, PacingError, PgPacingRepo, PoolView};
use ad_pacing::{Mode, Settings};

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/campaigns/:id/pacing", get(get_one).put(put_one))
        .route("/v1/agent-pools", get(list))
}

type ApiError = (StatusCode, Json<ErrorDto>);

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(ErrorDto { error: msg.into() }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default)]
    pool: Option<Settings>,
    #[serde(default)]
    reset: bool,
}

#[derive(Debug, Serialize)]
pub struct PacingDto {
    pub campaign_id: i64,
    pub mode: &'static str,
    /// The window limit of the campaign's next claims: the pool's `k`, with
    /// the mode and `max_k` applied.
    pub limit: u32,
    pub pool: PoolDto,
}

#[derive(Debug, Serialize)]
pub struct PoolDto {
    pub pool: String,
    /// The saved settings. An absent value takes its default.
    pub settings: Settings,
    /// The values in use.
    pub target_ms: u32,
    pub backoff_ms: u32,
    pub first_k: u32,
    pub max_k: Option<u32>,
    pub k: u32,
    pub slow_start: bool,
    /// The largest number of agent starts at once that all joined by the
    /// target.
    pub proven: u32,
    pub samples: u64,
    pub slow: u64,
    /// The pool's attempts whose agent is not in the room yet.
    pub window: i64,
    pub recent: RecentDto,
}

#[derive(Debug, Serialize)]
pub struct RecentDto {
    pub joins: u32,
    pub p50_ms: Option<u32>,
    pub p95_ms: Option<u32>,
    pub slow: u32,
}

impl From<PoolView> for PoolDto {
    fn from(v: PoolView) -> Self {
        PoolDto {
            pool: v.pool,
            settings: v.settings,
            target_ms: v.limits.target_ms,
            backoff_ms: v.limits.backoff_ms,
            first_k: v.limits.first_k,
            max_k: v.limits.max_k,
            k: v.state.k,
            slow_start: v.state.slow_start,
            proven: v.state.proven,
            samples: v.state.samples,
            slow: v.state.slow,
            window: v.window,
            recent: RecentDto {
                joins: v.recent.joins,
                p50_ms: v.recent.p50_ms,
                p95_ms: v.recent.p95_ms,
                slow: v.recent.slow,
            },
        }
    }
}

fn connect_within_ms() -> u32 {
    JurisdictionRule::CAMPAIGN.connect_within_ms().unwrap_or(0)
}

fn repo(state: &AppState) -> Result<&PgPacingRepo, ApiError> {
    state.pacing.as_ref().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "pacing needs Postgres (DATABASE_URL)",
        )
    })
}

fn storage(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// The campaign's pool, from its agent.
async fn pool_of(state: &AppState, id: i64) -> Result<String, ApiError> {
    let campaign = state
        .campaigns
        .get(CampaignId(id))
        .await
        .map_err(|e| match e {
            RepoError::NotFound(id) => {
                err(StatusCode::NOT_FOUND, format!("campaign {id} not found"))
            }
            RepoError::Storage(msg) => err(StatusCode::INTERNAL_SERVER_ERROR, msg),
        })?;
    campaign
        .agent
        .as_ref()
        .and_then(|a| AgentSpec::parse(a).ok())
        .map(|s| s.pool())
        .ok_or_else(|| {
            err(
                StatusCode::CONFLICT,
                format!("campaign {id} has no agent, so it has no agent pool"),
            )
        })
}

async fn view(pacing: &PgPacingRepo, id: i64, pool: &str) -> Result<PacingDto, ApiError> {
    let mode = pacing
        .mode(CampaignId(id))
        .await
        .map_err(storage)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("campaign {id} not found")))?;
    let v = pacing
        .view(pool, connect_within_ms())
        .await
        .map_err(|e| match e {
            // For example, a smaller abandonment limit makes a saved back-off
            // too large.
            PacingError::Settings(e) => err(
                StatusCode::CONFLICT,
                format!(
                    "{e}; the pool {pool} dials one call at a time; PUT new settings or a reset"
                ),
            ),
            e => storage(e),
        })?;
    Ok(PacingDto {
        campaign_id: id,
        mode: mode.name(),
        limit: ad_pacing::limit(&v.state, &v.limits, mode),
        pool: v.into(),
    })
}

async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<PacingDto>, ApiError> {
    let pacing = repo(&state)?;
    let pool = pool_of(&state, id).await?;
    Ok(Json(view(pacing, id, &pool).await?))
}

async fn put_one(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<PacingDto>, ApiError> {
    let pacing = repo(&state)?;
    let update: Update =
        serde_json::from_value(body).map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let pool = pool_of(&state, id).await?;
    if let Some(s) = &update.pool {
        s.resolve(connect_within_ms())
            .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    if let Some(mode) = update.mode {
        pacing
            .set_mode(CampaignId(id), mode)
            .await
            .map_err(storage)?;
    }
    if update.pool.is_some() || update.reset {
        let settings = match update.pool {
            Some(s) => s,
            None => match pacing.view(&pool, connect_within_ms()).await {
                Ok(v) => v.settings,
                // Settings that do not read: a reset puts the defaults back.
                Err(PacingError::Settings(_)) => Settings::default(),
                Err(e) => return Err(storage(e)),
            },
        };
        pacing
            .set_settings(&pool, &settings, update.reset)
            .await
            .map_err(storage)?;
    }
    Ok(Json(view(pacing, id, &pool).await?))
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum PoolItem {
    Pool(PoolDto),
    Invalid { pool: String, error: String },
}

async fn list(State(state): State<AppState>) -> Result<Json<Vec<PoolItem>>, ApiError> {
    let pools = repo(&state)?
        .list(connect_within_ms())
        .await
        .map_err(storage)?;
    Ok(Json(
        pools
            .into_iter()
            .map(|l| match l {
                Listed::Pool(v) => PoolItem::Pool(v.into()),
                Listed::Invalid { pool, error } => PoolItem::Invalid {
                    pool,
                    error: error.to_string(),
                },
            })
            .collect(),
    ))
}
