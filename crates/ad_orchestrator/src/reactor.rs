//! The pure event reactor (PLAN.md §0.10.8a). It maps an incoming
//! `CampaignEvent` to a `ReactorAction` using the breaker state. No I/O: the
//! binary performs the action (claim a batch, pause). This mirrors dograh's
//! `_handle_event`, kept pure so the control logic is unit-tested with no
//! database.
//!
//! Retries are not decided here. The orchestrator persists the retry decision
//! (`retry_decision`) with the call outcome, and `retry_needed` /
//! `retry_scheduled` events are informational.

use ad_events::CampaignEvent;

/// What the binary must do in response to an event.
#[derive(Debug, Clone, PartialEq)]
pub enum ReactorAction {
    /// Claim and dispatch the next batch for this campaign.
    ScheduleNextBatch { campaign_id: i64 },
    /// Pause dialing for this campaign (breaker open). No new batch.
    PauseCampaign { campaign_id: i64 },
    /// The campaign has no more work; stop.
    StopCampaign { campaign_id: i64 },
    /// Nothing to do (e.g. an informational event).
    Noop,
}

/// Inputs the reactor needs besides the event.
pub struct ReactorInput {
    /// Is the breaker open for this campaign right now?
    pub breaker_open: bool,
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
        // reacts to batch and breaker events, not the raw completion.
        CampaignEvent::CallCompleted { .. } => ReactorAction::Noop,
        // Informational: the retry is already persisted on the lead.
        CampaignEvent::RetryNeeded { .. } | CampaignEvent::RetryScheduled { .. } => {
            ReactorAction::Noop
        }
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
