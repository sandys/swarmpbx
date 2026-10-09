//! Orchestrator (PLAN.md §0.10 / §7). The singleton that decides *whether* a
//! campaign may dispatch a batch right now, gating on:
//!   1. campaign state (must be running),
//!   2. the schedule window (business hours — distinct from compliance),
//!   3. the circuit breaker (pause when the failure rate is too high).
//!
//! The decision is pure. Running the batch itself (claim + dispatch) is the
//! dispatcher's job; the orchestrator calls it only when the gate allows. The
//! singleton guarantee comes from a Postgres advisory lock (`ad_db::leader`),
//! applied by the binary that runs the orchestrator — not re-implemented here.

use chrono::{DateTime, Datelike, Utc, Weekday};
use chrono_tz::Tz;

use ad_campaign::CampaignState;
use ad_circuit_breaker::{CircuitBreaker, Config as BreakerConfig};

/// A business-hours schedule window: a timezone and per-weekday [start, end)
/// in minutes since local midnight. Distinct from the compliance safe-hours
/// gate (which is per-lead and fails closed); this is operator routing.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub timezone: String,
    /// Per-weekday (Mon..Sun) windows as (start_min, end_min). None = always on.
    pub windows: Option<[(u16, u16); 7]>,
}

impl Schedule {
    /// Always-dialable schedule (no window restriction).
    pub fn always() -> Self {
        Schedule {
            timezone: "UTC".into(),
            windows: None,
        }
    }

    /// Whether `now` falls inside the schedule. Unknown timezone fails OPEN
    /// here (business routing, not compliance): a misconfigured business window
    /// should not silently halt a campaign — the compliance gate still runs
    /// per-lead at dispatch and fails closed.
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        let Some(windows) = &self.windows else {
            return true;
        };
        let tz: Tz = match self.timezone.parse() {
            Ok(tz) => tz,
            Err(_) => return true, // fail open for business schedule
        };
        let local = now.with_timezone(&tz);
        let idx = weekday_index(local.weekday());
        let (start, end) = windows[idx];
        let minutes = (local
            .time()
            .format("%H")
            .to_string()
            .parse::<u16>()
            .unwrap_or(0))
            * 60
            + local
                .time()
                .format("%M")
                .to_string()
                .parse::<u16>()
                .unwrap_or(0);
        if start == end {
            return false; // closed day
        }
        if start <= end {
            minutes >= start && minutes < end
        } else {
            minutes >= start || minutes < end // wraps midnight
        }
    }
}

fn weekday_index(w: Weekday) -> usize {
    w.num_days_from_monday() as usize
}

/// Why a dispatch batch was not scheduled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateBlock {
    NotRunning,
    OutsideSchedule,
    CircuitOpen,
}

/// The scheduling decision for a campaign at a moment in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Dispatch is allowed; the orchestrator should claim + run a batch.
    Dispatch,
    /// Hold; the campaign should not dispatch now (with the reason).
    Hold(GateBlock),
}

/// The pure scheduling gate. Decides whether to dispatch a batch now.
pub fn gate(
    now: DateTime<Utc>,
    state: CampaignState,
    schedule: &Schedule,
    breaker: &CircuitBreaker,
    breaker_config: &BreakerConfig,
) -> GateDecision {
    if state != CampaignState::Running {
        return GateDecision::Hold(GateBlock::NotRunning);
    }
    if !schedule.is_open(now) {
        return GateDecision::Hold(GateBlock::OutsideSchedule);
    }
    let (open, _stats) = breaker.is_open(now, breaker_config);
    if open {
        return GateDecision::Hold(GateBlock::CircuitOpen);
    }
    GateDecision::Dispatch
}

#[cfg(test)]
mod tests;
