//! The pure event reactor (PLAN.md §0.10.8a). It maps an incoming
//! `CampaignEvent` to a `ReactorAction` using the breaker state and a retry
//! policy. No I/O: the binary performs the action (claim a batch, schedule a
//! due-timer, pause). This mirrors dograh's `_handle_event`, kept pure so the
//! control logic is unit-tested with no Redis or Postgres.

use ad_events::{CampaignEvent, RetryReason};

/// Retry timing policy. The interval depends on why the call failed, matching
/// VICIdial per-status retry rules.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub busy_ms: i64,
    pub no_answer_ms: i64,
    pub voicemail_ms: i64,
    pub failed_ms: i64,
    pub error_ms: i64,
    /// A retry is dropped once attempts reach this cap.
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // Minutes-scale defaults (ms); tune per campaign later.
        RetryPolicy {
            busy_ms: 5 * 60_000,
            no_answer_ms: 30 * 60_000,
            voicemail_ms: 60 * 60_000,
            failed_ms: 15 * 60_000,
            error_ms: 15 * 60_000,
            max_attempts: 3,
        }
    }
}

impl RetryPolicy {
    pub fn interval_ms(&self, reason: RetryReason) -> i64 {
        match reason {
            RetryReason::Busy => self.busy_ms,
            RetryReason::NoAnswer => self.no_answer_ms,
            RetryReason::Voicemail => self.voicemail_ms,
            RetryReason::Failed => self.failed_ms,
            RetryReason::Error => self.error_ms,
        }
    }
}

/// What the binary must do in response to an event.
#[derive(Debug, Clone, PartialEq)]
pub enum ReactorAction {
    /// Claim and dispatch the next batch for this campaign.
    ScheduleNextBatch { campaign_id: i64 },
    /// Put a lead on the due-timer schedule at `due_epoch_ms`.
    ScheduleRetry {
        campaign_id: i64,
        lead_id: i64,
        due_epoch_ms: i64,
    },
    /// Give up on a lead (max attempts reached): mark terminal, no retry.
    DropRetry { campaign_id: i64, lead_id: i64 },
    /// Pause dialing for this campaign (breaker open). No new batch.
    PauseCampaign { campaign_id: i64 },
    /// The campaign has no more work; stop.
    StopCampaign { campaign_id: i64 },
    /// Nothing to do (e.g. an informational event).
    Noop,
}

/// Inputs the reactor needs besides the event.
pub struct ReactorInput {
    /// `now` in epoch-ms, for computing retry due times.
    pub now_epoch_ms: i64,
    /// Is the breaker open for this campaign right now?
    pub breaker_open: bool,
    /// Attempts already made for the lead in a retry event (to enforce the cap).
    pub lead_attempts: u32,
    pub policy: RetryPolicy,
}

/// Decide the action for an event. Pure.
pub fn react(event: &CampaignEvent, input: &ReactorInput) -> ReactorAction {
    match event {
        // A batch finished: schedule the next one, unless the breaker is open.
        CampaignEvent::BatchCompleted { campaign_id, .. } => {
            if input.breaker_open {
                ReactorAction::PauseCampaign {
                    campaign_id: *campaign_id,
                }
            } else {
                ReactorAction::ScheduleNextBatch {
                    campaign_id: *campaign_id,
                }
            }
        }
        // A batch failed as a whole: do not immediately re-dispatch; wait for the
        // breaker/next event. (dograh does not fail the campaign on one batch.)
        CampaignEvent::BatchFailed { .. } => ReactorAction::Noop,
        // A call finished: this is recorded by the dispatch path; the reactor
        // reacts to the derived retry/breaker events, not the raw completion.
        CampaignEvent::CallCompleted { .. } => ReactorAction::Noop,
        // A terminal call needs a retry: schedule it at the policy interval,
        // unless the attempt cap is reached.
        CampaignEvent::RetryNeeded {
            campaign_id,
            lead_id,
            reason,
        } => {
            if input.lead_attempts >= input.policy.max_attempts {
                ReactorAction::DropRetry {
                    campaign_id: *campaign_id,
                    lead_id: *lead_id,
                }
            } else {
                let due = input.now_epoch_ms + input.policy.interval_ms(*reason);
                ReactorAction::ScheduleRetry {
                    campaign_id: *campaign_id,
                    lead_id: *lead_id,
                    due_epoch_ms: due,
                }
            }
        }
        // Informational: the retry is already on the schedule.
        CampaignEvent::RetryScheduled { .. } => ReactorAction::Noop,
        // The breaker tripped: pause.
        CampaignEvent::CircuitBreakerTripped { campaign_id, .. } => ReactorAction::PauseCampaign {
            campaign_id: *campaign_id,
        },
        // No more work.
        CampaignEvent::CampaignCompleted { campaign_id } => ReactorAction::StopCampaign {
            campaign_id: *campaign_id,
        },
    }
}

#[cfg(test)]
#[path = "reactor_tests.rs"]
mod reactor_tests;
