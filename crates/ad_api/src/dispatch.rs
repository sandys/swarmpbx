//! Dispatch-batch API — drives one batch through the dial loop with a stub
//! call initiator (the real SIP↔WebRTC router lands behind the same
//! `CallInitiator` seam later).
//!
//! EXPERIMENTAL: the initiator is a stub that answers every call, and the
//! breaker is new on every request. The response says so in `experimental` and
//! `initiator`. It places no real calls (GAPS.md API-3).
//!
//!   POST /v1/campaigns/:id/dispatch/run-batch
//!     body: { uniform_window|windows, holidays?, dnc?, max_concurrent? }
//!     -> { experimental, initiator, gate, dispatched, outcomes: [{lead_id, outcome}] }
//!
//! This proves the orchestrator gate + hopper selection + dispatch loop work
//! together on real leads: a non-running campaign is held; a running one dials
//! its eligible leads (compliance re-gated) up to the concurrency cap.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use ad_circuit_breaker::{CircuitBreaker, Config as BreakerConfig};
use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::{CallWindowSet, DayWindow};
use ad_core::CampaignId;
use ad_dispatch::{
    CallInitiator, CallOutcome, Capacity, ComplianceData, DialRequest, Dispatcher, Placement,
};
use ad_hopper::{select, Order, SelectionInput};
use ad_orchestrator::{gate, GateBlock, GateDecision, Schedule};

use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/campaigns/:id/dispatch/run-batch", post(run_batch))
}

// ---- stub initiator + capacity for the API (real router lands later) ----

struct StubInitiator;
impl CallInitiator for StubInitiator {
    async fn place(&self, _request: &DialRequest<'_>) -> Placement {
        // A stub: pretend every placed call is answered. The seam is what
        // matters; the real router returns real outcomes.
        Placement::Answered
    }
}

struct AtomicCapacity {
    in_use: Arc<AtomicUsize>,
    max: usize,
}
struct Slot {
    in_use: Arc<AtomicUsize>,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.in_use.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Capacity for AtomicCapacity {
    type Slot = Slot;
    fn try_acquire(&self) -> Option<Slot> {
        loop {
            let cur = self.in_use.load(Ordering::SeqCst);
            if cur >= self.max {
                return None;
            }
            if self
                .in_use
                .compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(Slot {
                    in_use: self.in_use.clone(),
                });
            }
        }
    }
}

// ---- DTOs ----

#[derive(Debug, Deserialize)]
pub struct DayWindowDto {
    pub start_hhmm: u16,
    pub stop_hhmm: u16,
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

#[derive(Debug, Deserialize)]
pub struct RunBatchRequest {
    pub uniform_window: Option<DayWindowDto>,
    pub windows: Option<Vec<DayWindowDto>>,
    #[serde(default)]
    pub holidays: Vec<String>,
    #[serde(default)]
    pub dnc: Option<DncDto>,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
}

fn default_max_concurrent() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct OutcomeDto {
    pub lead_id: i64,
    pub outcome: String,
}

#[derive(Debug, Serialize)]
pub struct RunBatchResponse {
    /// Always true: this endpoint does not place real calls.
    pub experimental: bool,
    /// The call initiator in use: "stub" answers every call.
    pub initiator: &'static str,
    pub gate: String,
    pub dispatched: usize,
    pub outcomes: Vec<OutcomeDto>,
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorDto>) {
    (status, Json(ErrorDto { error: msg.into() }))
}

fn build_windows(req: &RunBatchRequest) -> Result<CallWindowSet, String> {
    let mut set = if let Some(ws) = &req.windows {
        if ws.len() != 7 {
            return Err("windows must contain exactly 7 entries".into());
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

fn build_dnc(dto: Option<&DncDto>) -> DncSet {
    let mut set = DncSet::default();
    if let Some(d) = dto {
        set.system = d
            .system
            .iter()
            .map(|s| ad_compliance::dnc::normalize(s))
            .collect();
        set.campaign = d
            .campaign
            .iter()
            .map(|s| ad_compliance::dnc::normalize(s))
            .collect();
        set.area_codes = d.area_codes.iter().cloned().collect();
    }
    set
}

fn outcome_str(o: &CallOutcome) -> String {
    match o {
        CallOutcome::Answered => "answered".into(),
        CallOutcome::NotAnswered(r) => format!("not_answered:{r}"),
        CallOutcome::Failed(r) => format!("failed:{r}"),
        CallOutcome::ComplianceDenied(r) => format!("compliance_denied:{r}"),
        CallOutcome::NoCapacity => "no_capacity".into(),
    }
}

async fn run_batch(
    State(state): State<AppState>,
    Path(campaign_id): Path<i64>,
    Json(req): Json<RunBatchRequest>,
) -> Result<Json<RunBatchResponse>, (StatusCode, Json<ErrorDto>)> {
    let cid = CampaignId(campaign_id);
    let campaign = state.campaigns.get(cid).await.map_err(|_| {
        err(
            StatusCode::NOT_FOUND,
            format!("campaign {campaign_id} not found"),
        )
    })?;

    let now = state.clock.now_utc();

    // Orchestrator gate: state + schedule + circuit breaker. (Fresh breaker and
    // always-on schedule here; persistence of breaker state lands with the
    // orchestrator service.)
    let breaker = CircuitBreaker::new();
    let decision = gate(
        now,
        campaign.state,
        &Schedule::always(),
        &breaker,
        &BreakerConfig::default(),
    );
    if let GateDecision::Hold(block) = decision {
        let reason = match block {
            GateBlock::NotRunning => "not_running",
            GateBlock::OutsideSchedule => "outside_schedule",
            GateBlock::CircuitOpen => "circuit_open",
        };
        // 409: the request is well-formed but conflicts with the campaign's
        // current gate state.
        return Err(err(StatusCode::CONFLICT, format!("hold:{reason}")));
    }

    let windows = build_windows(&req)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("invalid_window: {e}")))?;
    let dnc = build_dnc(req.dnc.as_ref());

    // Select eligible leads via the hopper (compliance + status + ordering).
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
            call_count_limit: None,
            order: Order::UpCount,
            ruleset: None,
        },
    );

    // Dispatch the eligible leads through the loop (stub initiator, capacity cap).
    let initiator = StubInitiator;
    let capacity = AtomicCapacity {
        in_use: Arc::new(AtomicUsize::new(0)),
        max: req.max_concurrent,
    };
    let dispatcher = Dispatcher {
        clock: state.clock.as_ref(),
        initiator: &initiator,
        capacity: &capacity,
        compliance: ComplianceData {
            windows: &windows,
            dnc: &dnc,
        },
    };
    let results = dispatcher.dispatch_batch(&selection.eligible).await;

    let dispatched = results.iter().filter(|(_, o)| o.is_attempted()).count();
    Ok(Json(RunBatchResponse {
        experimental: true,
        initiator: "stub",
        gate: "dispatch".into(),
        dispatched,
        outcomes: results
            .into_iter()
            .map(|(lead_id, o)| OutcomeDto {
                lead_id,
                outcome: outcome_str(&o),
            })
            .collect(),
    }))
}
