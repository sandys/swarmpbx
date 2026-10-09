//! Campaign API: create, list, get, and lifecycle control.
//!
//! Endpoints:
//!   POST   /v1/campaigns                 create (201)
//!   GET    /v1/campaigns?org_id=N        list for an org
//!   GET    /v1/campaigns/:id             get one (404 if missing)
//!   POST   /v1/campaigns/:id/transition  apply a lifecycle transition
//!                                         (404 if missing, 409 if illegal)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use ad_campaign::repo::{NewCampaign, RepoError};
use ad_campaign::{Campaign, CampaignState, Counters, Transition};
use ad_core::{CampaignId, OrgId};

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/campaigns", post(create).get(list))
        .route("/v1/campaigns/:id", get(get_one))
        .route("/v1/campaigns/:id/transition", post(transition))
}

// ---- DTOs ----

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub org_id: i64,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CampaignDto {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub state: CampaignState,
    pub counters: Counters,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Campaign> for CampaignDto {
    fn from(c: Campaign) -> Self {
        CampaignDto {
            id: c.id.0,
            org_id: c.org_id.0,
            name: c.name,
            state: c.state,
            counters: c.counters,
            created_at: c.created_at.to_rfc3339(),
            updated_at: c.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub org_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct TransitionRequest {
    pub transition: Transition,
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorDto>) {
    (status, Json(ErrorDto { error: msg.into() }))
}

// ---- handlers ----

async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateRequest>,
) -> Result<(StatusCode, Json<CampaignDto>), (StatusCode, Json<ErrorDto>)> {
    if req.name.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name must not be empty"));
    }
    let campaign = state
        .campaigns
        .create(NewCampaign {
            org_id: OrgId(req.org_id),
            name: req.name,
        })
        .await
        .map_err(repo_err)?;
    Ok((StatusCode::CREATED, Json(campaign.into())))
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<CampaignDto>>, (StatusCode, Json<ErrorDto>)> {
    let campaigns = state
        .campaigns
        .list(OrgId(q.org_id))
        .await
        .map_err(repo_err)?;
    Ok(Json(campaigns.into_iter().map(CampaignDto::from).collect()))
}

async fn get_one(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CampaignDto>, (StatusCode, Json<ErrorDto>)> {
    let campaign = state
        .campaigns
        .get(CampaignId(id))
        .await
        .map_err(repo_err)?;
    Ok(Json(campaign.into()))
}

async fn transition(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TransitionRequest>,
) -> Result<Json<CampaignDto>, (StatusCode, Json<ErrorDto>)> {
    let mut campaign = state
        .campaigns
        .get(CampaignId(id))
        .await
        .map_err(repo_err)?;
    let now = state.clock.now_utc();
    // An illegal transition is a 409 Conflict, not a 400: the request is
    // well-formed but conflicts with the campaign's current state.
    campaign
        .apply(req.transition, now)
        .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    let saved = state.campaigns.save(campaign).await.map_err(repo_err)?;
    Ok(Json(saved.into()))
}

/// Map a repository error to an HTTP response.
fn repo_err(e: RepoError) -> (StatusCode, Json<ErrorDto>) {
    match e {
        RepoError::NotFound(id) => err(StatusCode::NOT_FOUND, format!("campaign {id} not found")),
        RepoError::Storage(msg) => err(StatusCode::INTERNAL_SERVER_ERROR, msg),
    }
}
