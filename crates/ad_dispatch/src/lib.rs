//! Dispatch — the dial seam and launch pacing (PLAN.md §7, §0.10).
//!
//! `CallInitiator` is the seam to the media plane: a stub in tests, the real
//! gateway (`ad_gateway`) in production. It places one call and reports a
//! `Placement`, which becomes an `ad_attempt` event.
//!
//! `Dispatcher` is the older loop that the experimental
//! `dispatch/run-batch` API uses: it re-gates on compliance, takes a
//! concurrency slot, and places the call. The orchestrator does not use it.

use ad_attempt::{classify_sip, AttemptEvent, Outcome};
use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::{self, CallWindowSet, LeadLocation};
use ad_compliance::Decision;
use ad_core::time::Clock;
use ad_lead::Lead;
use chrono::{DateTime, Utc};

mod pacer;
pub use pacer::Pacer;

/// One dial request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialRequest<'a> {
    /// The room that the answered call joins.
    pub room: &'a str,
    /// The identity of the customer leg in the room.
    pub participant_identity: &'a str,
    pub phone_number: &'a str,
}

/// The result of one dial request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// The callee answered.
    Answered,
    /// The provider returned a final result that is not an answer.
    Rejected {
        sip_code: Option<u16>,
        detail: String,
    },
    /// The request did not reach the provider. No call exists.
    NotSent(String),
    /// The request went out, but no result came back. A call may exist.
    NoResponse(String),
}

impl Placement {
    /// The attempt event for this result.
    pub fn into_event(self, at: DateTime<Utc>) -> AttemptEvent {
        match self {
            Placement::Answered => AttemptEvent::Answered { at },
            Placement::Rejected { sip_code, detail } => AttemptEvent::Rejected {
                at,
                sip_code,
                detail,
            },
            Placement::NotSent(detail) => AttemptEvent::NotSent { at, detail },
            Placement::NoResponse(detail) => AttemptEvent::NoResponse { at, detail },
        }
    }
}

/// The seam to the media plane.
pub trait CallInitiator: Send + Sync {
    fn place(
        &self,
        request: &DialRequest<'_>,
    ) -> impl std::future::Future<Output = Placement> + Send;
}

/// The result of `Dispatcher::dispatch_lead`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// The call connected (answered).
    Answered,
    /// Placed but not answered (busy or no answer).
    NotAnswered(String),
    /// The call failed.
    Failed(String),
    /// Not attempted: compliance denied at dispatch time.
    ComplianceDenied(String),
    /// Not attempted: no concurrency capacity available.
    NoCapacity,
}

impl CallOutcome {
    /// Whether this outcome counts as a failure for the circuit breaker.
    /// Compliance-denied and no-capacity are NOT failures (we never dialed).
    pub fn is_failure(&self) -> bool {
        matches!(self, CallOutcome::Failed(_))
    }
    pub fn is_attempted(&self) -> bool {
        matches!(
            self,
            CallOutcome::Answered | CallOutcome::NotAnswered(_) | CallOutcome::Failed(_)
        )
    }

    fn from_placement(p: Placement) -> Self {
        match p {
            Placement::Answered => CallOutcome::Answered,
            Placement::Rejected {
                sip_code: Some(code),
                detail,
            } => match classify_sip(code, detail.clone()) {
                Outcome::Busy { .. } | Outcome::NoAnswer { .. } => CallOutcome::NotAnswered(detail),
                _ => CallOutcome::Failed(detail),
            },
            Placement::Rejected { detail, .. }
            | Placement::NotSent(detail)
            | Placement::NoResponse(detail) => CallOutcome::Failed(detail),
        }
    }
}

/// A concurrency-capacity source. `acquire` returns a slot guard if capacity is
/// available, else None. Dropping the guard releases the slot (RAII), so a
/// panic or early return never leaks capacity.
pub trait Capacity: Send + Sync {
    type Slot: Send;
    fn try_acquire(&self) -> Option<Self::Slot>;
}

/// Compliance inputs supplied for the re-gate (loaded by the caller).
pub struct ComplianceData<'a> {
    pub windows: &'a CallWindowSet,
    pub dnc: &'a DncSet,
}

/// The dispatcher ties the seams together. Generic over the initiator and the
/// capacity source so production and tests share one code path.
pub struct Dispatcher<'a, I: CallInitiator, C: Capacity> {
    pub clock: &'a dyn Clock,
    pub initiator: &'a I,
    pub capacity: &'a C,
    pub compliance: ComplianceData<'a>,
}

impl<'a, I: CallInitiator, C: Capacity> Dispatcher<'a, I, C> {
    /// Dispatch a single lead through the full gate → capacity → initiate path.
    pub async fn dispatch_lead(&self, lead: &Lead) -> CallOutcome {
        // 1) Re-gate compliance at dispatch time (DNC then safe-hours).
        if let Decision::Denied(r) = self.compliance.dnc.check(&lead.phone_number) {
            return CallOutcome::ComplianceDenied(r.to_string());
        }
        let loc = LeadLocation {
            timezone: lead.timezone.clone(),
        };
        if let Decision::Denied(r) = safe_hours::check(self.clock, &loc, self.compliance.windows) {
            return CallOutcome::ComplianceDenied(r.to_string());
        }

        // 2) Acquire a concurrency slot. The guard releases on drop (RAII).
        let _slot = match self.capacity.try_acquire() {
            Some(s) => s,
            None => return CallOutcome::NoCapacity,
        };

        // 3) Place the call via the seam. The slot is held for the duration and
        //    released when `_slot` drops here.
        let room = format!("dispatch-{}", lead.id.0);
        let identity = format!("sip-lead-{}", lead.id.0);
        let placement = self
            .initiator
            .place(&DialRequest {
                room: &room,
                participant_identity: &identity,
                phone_number: &lead.phone_number,
            })
            .await;
        CallOutcome::from_placement(placement)
    }

    /// Dispatch a batch, returning (lead_id, outcome) in order. Stops acquiring
    /// once capacity is exhausted (the remaining leads report NoCapacity),
    /// which is the progressive-pacing behavior: never dial beyond capacity.
    pub async fn dispatch_batch(&self, leads: &[Lead]) -> Vec<(i64, CallOutcome)> {
        let mut out = Vec::with_capacity(leads.len());
        for lead in leads {
            let outcome = self.dispatch_lead(lead).await;
            out.push((lead.id.0, outcome));
        }
        out
    }
}

#[cfg(test)]
mod tests;
