//! Dispatch — the inner dial loop (PLAN.md §7, §0.10). For each claimed lead the
//! dispatcher: re-gates on compliance (the clock moved since hopper fill),
//! acquires a concurrency slot, initiates the call through the `CallInitiator`
//! seam, records the outcome, and releases the slot.
//!
//! `CallInitiator` is the seam to the media plane: a stub in tests, the real
//! SIP↔WebRTC router (`ad_gateway`) in production. The dispatch *logic* is the
//! subject here and is fully testable without any telephony.

use ad_compliance::dnc::DncSet;
use ad_compliance::safe_hours::{self, CallWindowSet, LeadLocation};
use ad_compliance::Decision;
use ad_core::time::Clock;
use ad_lead::Lead;

/// The outcome of a single dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// The call connected (answered).
    Answered,
    /// Placed but not answered (no-answer / busy / voicemail).
    NotAnswered(String),
    /// Could not place the call (initiator error).
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
}

/// The seam to the media plane. Implemented by a stub in tests and by the real
/// router (`ad_gateway.initiate_call`) in production.
pub trait CallInitiator: Send + Sync {
    fn initiate(&self, lead: &Lead) -> impl std::future::Future<Output = CallOutcome> + Send;
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

        // 3) Initiate the call via the seam, record the outcome. The slot is
        //    held for the duration and released when `_slot` drops here.
        self.initiator.initiate(lead).await
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
