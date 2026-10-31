//! The results of the calls: the disposition catalog, the DNC list, the
//! attempts, and the campaign events (GAPS.md LEAD-10, COMP-7, AGNT-2,
//! OBS-1).
//!
//! Endpoints (Postgres only; 503 without it):
//!   GET    /v1/campaigns/:id/dispositions  {"system": [...], "campaign": [...]}
//!   PUT    /v1/campaigns/:id/dispositions  [{"code", "name", "flags"}]: the
//!                                          campaign codes (400 if invalid)
//!   GET    /v1/campaigns/:id/attempts      ?after=<attempt id>&limit=<n>, with
//!                                          the disposition and the agent outcome
//!   GET    /v1/campaigns/:id/events        ?after=<event id>&limit=<n>
//!   GET    /v1/dnc                         ?campaign_id=<id>: the system list
//!                                          and that campaign's list, newest first
//!   POST   /v1/dnc                         {"phone_number", "scope", "campaign_id",
//!                                          "region", "note"}: 201, or 200 when
//!                                          the entry exists
//!   DELETE /v1/dnc/:id                     204 (404 if missing)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use ad_core::CampaignId;
use ad_db::{AttemptView, DncEntry, NewDnc};
use ad_disposition::{catalog, Catalog, Disposition, DncScope};

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/campaigns/:id/dispositions",
            get(get_catalog).put(put_catalog),
        )
        .route("/v1/campaigns/:id/attempts", get(attempts))
        .route("/v1/campaigns/:id/events", get(events))
        .route("/v1/dnc", get(list_dnc).post(add_dnc))
        .route("/v1/dnc/:id", delete(remove_dnc))
}

type ApiError = (StatusCode, Json<ErrorDto>);

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(ErrorDto { error: msg.into() }))
}

fn storage(e: impl std::fmt::Display) -> ApiError {
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn db(state: &AppState) -> Result<&ad_db::Db, ApiError> {
    state
        .db
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "needs Postgres"))
}

async fn campaign(state: &AppState, id: i64) -> Result<CampaignId, ApiError> {
    let cid = CampaignId(id);
    state
        .campaigns
        .get(cid)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, format!("campaign {id} not found")))?;
    Ok(cid)
}

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    100
}

fn limit(p: &Page) -> i64 {
    p.limit.clamp(1, 1000)
}

#[derive(Debug, Serialize)]
pub struct CatalogDto {
    pub system: Vec<Disposition>,
    pub campaign: Vec<Disposition>,
}

async fn get_catalog(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CatalogDto>, ApiError> {
    let db = db(&state)?;
    let cid = campaign(&state, id).await?;
    Ok(Json(CatalogDto {
        system: catalog::system(),
        campaign: db.outcomes().catalog(cid).await.map_err(storage)?,
    }))
}

async fn put_catalog(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(codes): Json<Vec<Disposition>>,
) -> Result<Json<CatalogDto>, ApiError> {
    let db = db(&state)?;
    let cid = campaign(&state, id).await?;
    let checked = Catalog::new(codes).map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    db.outcomes()
        .put_catalog(cid, checked.campaign())
        .await
        .map_err(storage)?;
    Ok(Json(CatalogDto {
        system: catalog::system(),
        campaign: checked.campaign().to_vec(),
    }))
}

async fn attempts(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<AttemptView>>, ApiError> {
    let db = db(&state)?;
    let cid = campaign(&state, id).await?;
    let rows = db
        .attempts()
        .page(cid, page.after, limit(&page))
        .await
        .map_err(storage)?;
    Ok(Json(rows))
}

#[derive(Debug, Serialize)]
pub struct EventDto {
    pub id: i64,
    pub at: String,
    pub kind: &'static str,
    pub event: ad_events::CampaignEvent,
}

async fn events(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(page): Query<Page>,
) -> Result<Json<Vec<EventDto>>, ApiError> {
    let db = db(&state)?;
    let cid = campaign(&state, id).await?;
    let rows = db
        .events()
        .page(cid.0, page.after, limit(&page))
        .await
        .map_err(storage)?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, at, event)| EventDto {
                id,
                at: at.to_rfc3339(),
                kind: event.kind(),
                event,
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
struct DncQuery {
    #[serde(default)]
    campaign_id: Option<i64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn list_dnc(
    State(state): State<AppState>,
    Query(q): Query<DncQuery>,
) -> Result<Json<Vec<DncEntry>>, ApiError> {
    let db = db(&state)?;
    let rows = db
        .dnc()
        .list(q.campaign_id.map(CampaignId), q.limit.clamp(1, 1000))
        .await
        .map_err(storage)?;
    Ok(Json(rows))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewDncDto {
    phone_number: String,
    /// `system` (the default) or `campaign`.
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    campaign_id: Option<i64>,
    /// The country (ISO 3166) of a number without a country code.
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AddedDto {
    pub phone_number: String,
    pub created: bool,
    /// The waiting leads that the entry stopped.
    pub cancelled: u64,
}

async fn add_dnc(
    State(state): State<AppState>,
    Json(dto): Json<NewDncDto>,
) -> Result<(StatusCode, Json<AddedDto>), ApiError> {
    let db = db(&state)?;
    let bad = |m: String| err(StatusCode::BAD_REQUEST, m);
    let scope = match dto.scope.as_deref() {
        None => DncScope::System,
        Some(s) => {
            DncScope::parse(s).ok_or_else(|| bad(format!("scope '{s}': use system or campaign")))?
        }
    };
    let campaign_id = match (scope, dto.campaign_id) {
        (DncScope::Campaign, Some(id)) => Some(campaign(&state, id).await?),
        (DncScope::Campaign, None) => return Err(bad("scope campaign needs campaign_id".into())),
        (DncScope::System, Some(_)) => return Err(bad("scope system takes no campaign_id".into())),
        (DncScope::System, None) => None,
    };
    let phone = ad_contact::phone(&dto.phone_number, dto.region.as_deref())
        .map_err(|e| bad(format!("phone_number: {e}")))?;
    let added = db
        .dnc()
        .add(&NewDnc {
            phone_number: phone.e164.clone(),
            scope,
            campaign_id,
            source: "api",
            attempt_id: None,
            evidence: serde_json::json!({ "note": dto.note }),
        })
        .await
        .map_err(storage)?;
    let status = if added.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(AddedDto {
            phone_number: phone.e164.into_string(),
            created: added.created,
            cancelled: added.cancelled,
        }),
    ))
}

async fn remove_dnc(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let db = db(&state)?;
    if db.dnc().remove(id).await.map_err(storage)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(err(
            StatusCode::NOT_FOUND,
            format!("DNC entry {id} not found"),
        ))
    }
}
