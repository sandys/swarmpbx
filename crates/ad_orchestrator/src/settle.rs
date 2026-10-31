//! Settlement: what an ended call attempt does to its lead (PLAN.md §0.10.7).
//! Pure. The orchestrator saves the result in one transaction with the
//! attempt's `settled_at`, so each attempt updates its lead once.

use ad_attempt::{NotDialedReason, Outcome};
use ad_disposition::AgentOutcome;
use ad_events::RetryReason;
use ad_lead::LeadStatus;
use chrono::{DateTime, Utc};

use crate::retry::{retry_decision, RetryDecision, RetryPolicy};

/// The lead update for one ended attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settlement {
    pub status: LeadStatus,
    /// The attempt placed a call, so it counts in `called_count`.
    pub counts_as_call: bool,
    /// The lead is not dialable before this time.
    pub next_eligible_at: Option<DateTime<Utc>>,
    /// Why the lead waits (`leads.retry_reason`).
    pub wait_reason: Option<&'static str>,
    /// Set when a retry is scheduled.
    pub retry: Option<(DateTime<Utc>, RetryReason)>,
    /// The retry cap stopped a retry.
    pub exhausted: bool,
    /// The claim takes leads of a higher priority first.
    pub priority: i16,
    /// Set when the agent scheduled a callback.
    pub callback: Option<DateTime<Utc>>,
}

/// The priority of a lead with a callback (VICIdial puts callbacks at 50).
pub const CALLBACK_PRIORITY: i16 = 50;

impl Settlement {
    fn status(status: LeadStatus, counts_as_call: bool) -> Self {
        Settlement {
            status,
            counts_as_call,
            next_eligible_at: None,
            wait_reason: None,
            retry: None,
            exhausted: false,
            priority: 0,
            callback: None,
        }
    }
}

/// Decide the lead update of an attempt without an agent outcome.
/// `called_count` is the count before this attempt.
pub fn settle(
    outcome: &Outcome,
    called_count: u32,
    policy: &RetryPolicy,
    now: DateTime<Utc>,
) -> Settlement {
    settle_with(outcome, None, called_count, policy, now)
}

/// Decide the lead update from the end of the call and the agent's outcome.
/// A DNC request always wins: the lead is never dialed again. The other parts
/// of the outcome count only when the callee answered: a callback queues the
/// lead at its time with a higher priority, and a machine answer is a
/// voicemail retry.
pub fn settle_with(
    outcome: &Outcome,
    agent: Option<&AgentOutcome>,
    called_count: u32,
    policy: &RetryPolicy,
    now: DateTime<Utc>,
) -> Settlement {
    let waiting = if called_count == 0 {
        LeadStatus::New
    } else {
        LeadStatus::Queued
    };
    let retry = |reason: RetryReason| match retry_decision(reason, called_count + 1, policy, now) {
        RetryDecision::RetryAt(due) => Settlement {
            next_eligible_at: Some(due),
            wait_reason: Some(reason.as_str()),
            retry: Some((due, reason)),
            ..Settlement::status(LeadStatus::Queued, true)
        },
        RetryDecision::Exhausted => Settlement {
            exhausted: true,
            ..Settlement::status(LeadStatus::Dispositioned, true)
        },
    };
    let answered = matches!(outcome, Outcome::Completed { .. });
    match agent {
        Some(a) if a.dnc.is_some() => {
            let placed = !matches!(outcome, Outcome::NotDialed { .. });
            return Settlement::status(LeadStatus::Dnc, placed);
        }
        Some(a) if answered => {
            if let Some(cb) = &a.callback {
                return Settlement {
                    next_eligible_at: Some(cb.at_utc),
                    wait_reason: Some("callback"),
                    priority: CALLBACK_PRIORITY,
                    callback: Some(cb.at_utc),
                    ..Settlement::status(LeadStatus::Queued, true)
                };
            }
            if a.machine() {
                return retry(RetryReason::Voicemail);
            }
        }
        _ => {}
    }
    match outcome {
        Outcome::NotDialed { reason, .. } => match reason {
            NotDialedReason::Window => Settlement {
                next_eligible_at: Some(now + policy.window_recheck),
                wait_reason: Some("window"),
                ..Settlement::status(waiting, false)
            },
            NotDialedReason::Released => Settlement::status(waiting, false),
            NotDialedReason::NoAgent => Settlement {
                next_eligible_at: Some(now + policy.agent_recheck),
                wait_reason: Some("no_agent"),
                ..Settlement::status(waiting, false)
            },
            NotDialedReason::Dnc => Settlement::status(LeadStatus::Dnc, false),
            NotDialedReason::UnknownTimezone => Settlement::status(LeadStatus::Blocked, false),
            NotDialedReason::Excluded => Settlement::status(LeadStatus::Done, false),
        },
        Outcome::Completed { .. } | Outcome::Declined { .. } => {
            Settlement::status(LeadStatus::Dispositioned, true)
        }
        Outcome::InvalidNumber { .. } => Settlement::status(LeadStatus::Done, true),
        Outcome::Busy { .. } => retry(RetryReason::Busy),
        Outcome::NoAnswer { .. } => retry(RetryReason::NoAnswer),
        Outcome::Failed { .. } => retry(RetryReason::Failed),
        Outcome::Abandoned { .. } => retry(RetryReason::Abandoned),
        Outcome::SetupError { .. } | Outcome::Uncertain { .. } => retry(RetryReason::Error),
    }
}

#[cfg(test)]
#[path = "settle_tests.rs"]
mod settle_tests;
