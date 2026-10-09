//! Lead API: bulk-load a campaign's contact list, and list a campaign's leads.
//!
//!   POST /v1/campaigns/:id/leads   bulk load (201), 404 if campaign missing,
//!                                   400 if any phone_number is empty
//!   GET  /v1/campaigns/:id/leads   list the campaign's leads

use std::collections::BTreeMap;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use ad_core::CampaignId;
use ad_lead::repo::NewLead;
use ad_lead::{Lead, LeadStatus};

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/campaigns/:id/leads", post(bulk_load).get(list))
}

#[derive(Debug, Deserialize)]
pub struct NewLeadDto {
    pub phone_number: String,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    /// CSV extra columns → agent template variables.
    #[serde(default)]
    pub context: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct BulkLoadRequest {
    pub leads: Vec<NewLeadDto>,
}

#[derive(Debug, Serialize)]
pub struct LeadDto {
    pub id: i64,
    pub campaign_id: i64,
    pub phone_number: String,
    pub timezone: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub status: LeadStatus,
    pub called_count: u32,
    pub context: BTreeMap<String, String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<Lead> for LeadDto {
    fn from(l: Lead) -> Self {
        LeadDto {
            id: l.id.0,
            campaign_id: l.campaign_id.0,
            phone_number: l.phone_number,
            timezone: l.timezone,
            state: l.state,
            postal_code: l.postal_code,
            status: l.status,
            called_count: l.called_count,
            context: l.context,
            created_at: l.created_at.to_rfc3339(),
            updated_at: l.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BulkLoadResponse {
    pub created: usize,
    pub leads: Vec<LeadDto>,
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorDto>) {
    (status, Json(ErrorDto { error: msg.into() }))
}

async fn bulk_load(
    State(state): State<AppState>,
    Path(campaign_id): Path<i64>,
    Json(req): Json<BulkLoadRequest>,
) -> Result<(StatusCode, Json<BulkLoadResponse>), (StatusCode, Json<ErrorDto>)> {
    // The campaign must exist (FK + a clean 404 rather than a storage error).
    let cid = CampaignId(campaign_id);
    state.campaigns.get(cid).await.map_err(|_| {
        err(
            StatusCode::NOT_FOUND,
            format!("campaign {campaign_id} not found"),
        )
    })?;

    if req.leads.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "leads must not be empty"));
    }
    let mut new = Vec::with_capacity(req.leads.len());
    for (i, dto) in req.leads.into_iter().enumerate() {
        if dto.phone_number.trim().is_empty() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("lead {i}: phone_number is empty"),
            ));
        }
        new.push(NewLead {
            campaign_id: cid,
            phone_number: dto.phone_number,
            timezone: dto.timezone,
            state: dto.state,
            postal_code: dto.postal_code,
            context: dto.context,
        });
    }

    let created = state
        .leads
        .bulk_create(new)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((
        StatusCode::CREATED,
        Json(BulkLoadResponse {
            created: created.len(),
            leads: created.into_iter().map(LeadDto::from).collect(),
        }),
    ))
}

async fn list(
    State(state): State<AppState>,
    Path(campaign_id): Path<i64>,
) -> Result<Json<Vec<LeadDto>>, (StatusCode, Json<ErrorDto>)> {
    let cid = CampaignId(campaign_id);
    state.campaigns.get(cid).await.map_err(|_| {
        err(
            StatusCode::NOT_FOUND,
            format!("campaign {campaign_id} not found"),
        )
    })?;
    let leads = state
        .leads
        .list_by_campaign(cid)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(leads.into_iter().map(LeadDto::from).collect()))
}
