//! Campaign API: create, list, get, and lifecycle control.
//!
//! Endpoints:
//!   POST   /v1/campaigns                 create (201)
//!   GET    /v1/campaigns?org_id=N        list for an org
//!   GET    /v1/campaigns/:id             get one (404 if missing)
//!   POST   /v1/campaigns/:id/transition  apply a lifecycle transition
//!                                         (404 if missing, 409 if illegal)
//!   GET    /v1/campaigns/:id/abandonment the abandonment ledger: answered
//!                                         and abandoned calls in the rule's
//!                                         window, and whether customer first
//!                                         is allowed (404 if missing, 503
//!                                         without Postgres)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use ad_agent::AgentSpec;
use ad_campaign::repo::{NewCampaign, RepoError};
use ad_campaign::{Campaign, CampaignState, Counters, Transition, DEFAULT_CONCURRENCY_CAP};
use ad_compliance::abandon::JurisdictionRule;
use ad_core::{CampaignId, OrgId};

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/campaigns", post(create).get(list))
        .route("/v1/campaigns/:id", get(get_one))
        .route("/v1/campaigns/:id/transition", post(transition))
        .route("/v1/campaigns/:id/abandonment", get(abandonment))
}

// ---- DTOs ----

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub org_id: i64,
    pub name: String,
    /// The most live calls at one time. Default: 10.
    #[serde(default)]
    pub concurrency_cap: Option<u32>,
    /// The agent of the campaign, for example
    /// `{"runtime": "livekit", "agent_name": "sales"}` or
    /// `{"runtime": "pipecat", "start_url": "http://bots:7860/start"}`.
    #[serde(default)]
    pub agent: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct CampaignDto {
    pub id: i64,
    pub org_id: i64,
    pub name: String,
    pub state: CampaignState,
    pub counters: Counters,
    pub concurrency_cap: u32,
    pub agent: Option<serde_json::Value>,
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
            concurrency_cap: c.concurrency_cap,
            agent: c.agent,
            created_at: c.created_at.to_rfc3339(),
            updated_at: c.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AbandonmentDto {
    pub campaign_id: i64,
    /// The jurisdiction rule (`us_tsr` for every campaign now).
    pub rule: &'static str,
    pub window_days: i64,
    pub answered: u64,
    pub abandoned: u64,
    /// `null` when no call was answered in the window.
    pub rate: Option<f64>,
    pub cap: f64,
    /// Customer first: the longest time from the answer to the agent.
    pub connect_within_ms: i64,
    /// The campaign agent's dial order, or `null` without an agent.
    pub dial_order: Option<&'static str>,
    /// Whether the rate lets the next calls dial the customer first. When it
    /// is false, a `customer_first` campaign dials agent first.
    pub customer_first_allowed: bool,
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
    let concurrency_cap = req.concurrency_cap.unwrap_or(DEFAULT_CONCURRENCY_CAP);
    if concurrency_cap == 0 {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "concurrency_cap must be at least 1",
        ));
    }
    if let Some(agent) = &req.agent {
        AgentSpec::parse(agent).map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    }
    let campaign = state
        .campaigns
        .create(NewCampaign {
            org_id: OrgId(req.org_id),
            name: req.name,
            concurrency_cap,
            agent: req.agent,
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
    let now = state.clock.now_utc();
    // An illegal transition is a 409 Conflict, not a 400: the request is
    // well-formed but conflicts with the campaign's current state.
    let saved = state
        .campaigns
        .transition(CampaignId(id), req.transition, now)
        .await
        .map_err(repo_err)?
        .map_err(|e| err(StatusCode::CONFLICT, e.to_string()))?;
    Ok(Json(saved.into()))
}

async fn abandonment(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<AbandonmentDto>, (StatusCode, Json<ErrorDto>)> {
    let Some(attempts) = &state.attempts else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the abandonment ledger needs Postgres (DATABASE_URL)",
        ));
    };
    let campaign = state
        .campaigns
        .get(CampaignId(id))
        .await
        .map_err(repo_err)?;
    let rule = JurisdictionRule::CAMPAIGN;
    let counts = attempts
        .abandon_counts(campaign.id, state.clock.now_utc() - rule.window())
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let dial_order = campaign
        .agent
        .as_ref()
        .and_then(|a| AgentSpec::parse(a).ok())
        .map(|s| s.dial_order().name());
    Ok(Json(AbandonmentDto {
        campaign_id: id,
        rule: rule.name(),
        window_days: rule.window().num_days(),
        answered: counts.answered,
        abandoned: counts.abandoned,
        rate: counts.rate(),
        cap: rule.cap(),
        connect_within_ms: rule.connect_within().num_milliseconds(),
        dial_order,
        customer_first_allowed: rule.allows_customer_first(&counts),
    }))
}

/// Map a repository error to an HTTP response.
fn repo_err(e: RepoError) -> (StatusCode, Json<ErrorDto>) {
    match e {
        RepoError::NotFound(id) => err(StatusCode::NOT_FOUND, format!("campaign {id} not found")),
        RepoError::Storage(msg) => err(StatusCode::INTERNAL_SERVER_ERROR, msg),
    }
}
