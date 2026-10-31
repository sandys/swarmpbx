//! Hopper preview API — ties routing + compliance + leads together.
//!
//!   POST /v1/campaigns/:id/hopper/preview
//!     body: { uniform_window | windows, holidays?, dnc?, call_count_limit?,
//!             order?, ruleset? }
//!     -> { eligible: [LeadBrief], excluded: [{lead, reason}] }
//!
//! It runs `ad_hopper::select` over the campaign's leads (loaded from the lead
//! store) using the current clock, so operators can see exactly which leads
//! would dial now and why the rest are held — the end-to-end proof that the
//! routing engine and the hopper work together on real data.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use ad_compliance::safe_hours::{CallWindowSet, DayWindow};
use ad_core::CampaignId;
use ad_hopper::{select, ExcludeReason, Order, SelectionInput};
use ad_routing::{Break, Detail, Ruleset, Tag};

use crate::compliance::DncDto;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/campaigns/:id/hopper/preview", post(preview))
}

// ---- request DTOs ----

#[derive(Debug, Deserialize)]
pub struct DayWindowDto {
    pub start_hhmm: u16,
    pub stop_hhmm: u16,
}

/// A routing-rule detail in the request (mirrors ad_routing::Detail).
#[derive(Debug, Deserialize)]
pub struct DetailDto {
    pub tag: String, // "condition" | "action" | "anti_action"
    pub field_or_verb: String,
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub group: u32,
    #[serde(default)]
    pub order: u32,
    #[serde(default)]
    pub break_: Option<String>, // "on_true"|"on_false"|"always"|"never"
    #[serde(default)]
    pub inline: bool,
}

#[derive(Debug, Deserialize)]
pub struct PreviewRequest {
    pub uniform_window: Option<DayWindowDto>,
    pub windows: Option<Vec<DayWindowDto>>,
    #[serde(default)]
    pub holidays: Vec<String>,
    #[serde(default)]
    pub dnc: Option<DncDto>,
    #[serde(default)]
    pub call_count_limit: Option<u32>,
    #[serde(default)]
    pub order: Option<String>, // "up_count" | "oldest"
    #[serde(default)]
    pub ruleset: Vec<DetailDto>,
}

// ---- response DTOs ----

#[derive(Debug, Serialize)]
pub struct LeadBrief {
    pub id: i64,
    pub phone_number: String,
    pub called_count: u32,
}

#[derive(Debug, Serialize)]
pub struct ExcludedBrief {
    pub id: i64,
    pub phone_number: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    pub eligible: Vec<LeadBrief>,
    pub excluded: Vec<ExcludedBrief>,
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorDto>) {
    (status, Json(ErrorDto { error: msg.into() }))
}

fn build_windows(req: &PreviewRequest) -> Result<CallWindowSet, String> {
    let mut set = if let Some(ws) = &req.windows {
        if ws.len() != 7 {
            return Err("windows must contain exactly 7 entries (Mon..Sun)".into());
        }
        let mut by_weekday = [DayWindow::CLOSED; 7];
        for (i, w) in ws.iter().enumerate() {
            by_weekday[i] = DayWindow::from_hhmm(w.start_hhmm, w.stop_hhmm)
                .ok_or_else(|| format!("invalid HHMM in window {i}"))?;
        }
        CallWindowSet {
            by_weekday,
            holidays: vec![],
        }
    } else if let Some(u) = &req.uniform_window {
        CallWindowSet::uniform(u.start_hhmm, u.stop_hhmm)
            .ok_or_else(|| "invalid uniform_window HHMM".to_string())?
    } else {
        return Err("either windows or uniform_window is required".into());
    };
    for h in &req.holidays {
        let d = chrono::NaiveDate::parse_from_str(h, "%Y-%m-%d")
            .map_err(|_| format!("invalid holiday date: {h}"))?;
        set.holidays.push(d);
    }
    Ok(set)
}

fn build_ruleset(details: &[DetailDto]) -> Result<Option<Ruleset>, String> {
    if details.is_empty() {
        return Ok(None);
    }
    let mut rows = Vec::with_capacity(details.len());
    for d in details {
        let tag = match d.tag.as_str() {
            "condition" => Tag::Condition,
            "action" => Tag::Action,
            "anti_action" => Tag::AntiAction,
            other => return Err(format!("invalid detail tag: {other}")),
        };
        let break_ = match d.break_.as_deref() {
            Some("on_true") => Break::OnTrue,
            Some("on_false") => Break::OnFalse,
            Some("always") => Break::Always,
            None | Some("never") => Break::Never,
            Some(other) => return Err(format!("invalid break: {other}")),
        };
        let detail = Detail {
            tag,
            field_or_verb: d.field_or_verb.clone(),
            data: d.data.clone(),
            group: d.group,
            order: d.order,
            break_,
            inline: d.inline,
        };
        rows.push(detail);
    }
    Ok(Some(Ruleset::new(rows)))
}

fn reason_str(r: &ExcludeReason) -> String {
    match r {
        ExcludeReason::IneligibleStatus => "ineligible_status".into(),
        ExcludeReason::CallCountExceeded => "call_count_exceeded".into(),
        ExcludeReason::Compliance(d) => format!("compliance:{d}"),
        ExcludeReason::RoutingExcluded(v) => format!("routing:{v}"),
    }
}

async fn preview(
    State(state): State<AppState>,
    Path(campaign_id): Path<i64>,
    Json(req): Json<PreviewRequest>,
) -> Result<Json<PreviewResponse>, (StatusCode, Json<ErrorDto>)> {
    let cid = CampaignId(campaign_id);
    state.campaigns.get(cid).await.map_err(|_| {
        err(
            StatusCode::NOT_FOUND,
            format!("campaign {campaign_id} not found"),
        )
    })?;

    let windows = build_windows(&req)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid_window: {e}")))?;
    let dnc = DncDto::to_set(req.dnc.as_ref())
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid_dnc: {e}")))?;
    let ruleset = build_ruleset(&req.ruleset)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid_ruleset: {e}")))?;
    let order = match req.order.as_deref() {
        Some("oldest") => Order::Oldest,
        None | Some("up_count") => Order::UpCount,
        Some(other) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("invalid order: {other}"),
            ))
        }
    };

    let leads = state
        .leads
        .list_by_campaign(cid)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let selection = select(
        state.clock.as_ref(),
        SelectionInput {
            leads,
            windows: &windows,
            dnc: &dnc,
            call_count_limit: req.call_count_limit,
            order,
            ruleset: ruleset.as_ref(),
        },
    );

    Ok(Json(PreviewResponse {
        eligible: selection
            .eligible
            .into_iter()
            .map(|l| LeadBrief {
                id: l.id.0,
                phone_number: l.phone_number,
                called_count: l.called_count,
            })
            .collect(),
        excluded: selection
            .excluded
            .into_iter()
            .map(|e| ExcludedBrief {
                id: e.lead.id.0,
                phone_number: e.lead.phone_number,
                reason: reason_str(&e.reason),
            })
            .collect(),
    }))
}
