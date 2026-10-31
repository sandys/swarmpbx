//! Compliance API: expose the `ad_compliance` engine over HTTP.
//!
//! `POST /v1/compliance/check` runs the safe-hours gate (and optional DNC
//! scrub) for a lead *right now* (per the injected clock) and returns the
//! decision. This is the endpoint dispatch/orchestrator and external callers
//! use to ask "may I call this lead at this moment?".

use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

use ad_compliance::dnc::{DncEntryError, DncSet};
use ad_compliance::safe_hours::{self, CallWindowSet, DayWindow, LeadLocation};
use ad_compliance::{Decision, DenyReason};

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/compliance/check", post(check))
}

/// One per-weekday window as HHMM integers (e.g. 900, 2100). start==stop = closed.
#[derive(Debug, Deserialize)]
pub struct DayWindowDto {
    pub start_hhmm: u16,
    pub stop_hhmm: u16,
}

#[derive(Debug, Deserialize)]
pub struct CheckRequest {
    /// IANA timezone name for the lead, e.g. "America/New_York". Missing/invalid
    /// => fail closed (deny with unknown_local_time).
    pub timezone: Option<String>,
    pub phone_number: String,
    /// Per-weekday windows, Mon..Sun. If omitted, a uniform window is required.
    pub windows: Option<Vec<DayWindowDto>>,
    /// Uniform window used when `windows` is absent.
    pub uniform_window: Option<DayWindowDto>,
    /// Holiday dates (YYYY-MM-DD) on which no calling is allowed.
    #[serde(default)]
    pub holidays: Vec<String>,
    /// Optional DNC data to scrub against.
    #[serde(default)]
    pub dnc: Option<DncDto>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DncDto {
    #[serde(default)]
    pub system: Vec<String>,
    #[serde(default)]
    pub campaign: Vec<String>,
    #[serde(default)]
    pub area_codes: Vec<String>,
}

impl DncDto {
    /// The DNC set of a request. An entry that does not read is an error.
    pub fn to_set(dto: Option<&DncDto>) -> Result<DncSet, DncEntryError> {
        match dto {
            Some(d) => DncSet::from_lists(&d.system, &d.campaign, &d.area_codes),
            None => Ok(DncSet::default()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CheckResponse {
    pub allowed: bool,
    /// Machine-readable deny reason when `allowed` is false.
    pub reason: Option<String>,
}

/// Build a CallWindowSet from the request, or an error string (=> 400, which is
/// itself a fail-closed outcome: a malformed window never yields "allowed").
fn build_windows(req: &CheckRequest) -> Result<CallWindowSet, String> {
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

fn reason_str(r: &DenyReason) -> &'static str {
    match r {
        DenyReason::UnknownLocalTime => "unknown_local_time",
        DenyReason::OutsideCallingWindow => "outside_calling_window",
        DenyReason::Holiday => "holiday",
        DenyReason::Dnc(_) => "dnc",
        DenyReason::InvalidWindowConfig => "invalid_window_config",
        DenyReason::InvalidNumber => "invalid_number",
    }
}

async fn check(
    State(state): State<AppState>,
    Json(req): Json<CheckRequest>,
) -> Result<Json<CheckResponse>, (axum::http::StatusCode, Json<CheckResponse>)> {
    // Malformed window config => 400. This is fail-closed: we never return
    // allowed=true for input we could not interpret.
    let windows = build_windows(&req).map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            Json(CheckResponse {
                allowed: false,
                reason: Some(format!("invalid_window_config: {e}")),
            }),
        )
    })?;

    // DNC first (cheapest, and a DNC hit denies regardless of time).
    if let Some(dnc) = &req.dnc {
        let set = DncDto::to_set(Some(dnc)).map_err(|e| {
            (
                axum::http::StatusCode::BAD_REQUEST,
                Json(CheckResponse {
                    allowed: false,
                    reason: Some(format!("invalid_dnc: {e}")),
                }),
            )
        })?;
        if let Decision::Denied(r) = set.check(&req.phone_number) {
            return Ok(Json(CheckResponse {
                allowed: false,
                reason: Some(reason_str(&r).into()),
            }));
        }
    }

    let loc = LeadLocation {
        timezone: req.timezone.clone(),
    };
    let decision = safe_hours::check(state.clock.as_ref(), &loc, &windows);

    Ok(Json(match decision {
        Decision::Allowed => CheckResponse {
            allowed: true,
            reason: None,
        },
        Decision::Denied(r) => CheckResponse {
            allowed: false,
            reason: Some(reason_str(&r).into()),
        },
    }))
}
