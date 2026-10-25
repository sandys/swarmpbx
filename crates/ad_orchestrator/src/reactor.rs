//! The pure event reactor (PLAN.md §0.10.8a). It maps an incoming
//! `CampaignEvent` to a `ReactorAction`. No I/O: the binary performs the
//! action. The other events are records for operators and reports.
//!
//! The circuit breaker is not an input here. Dispatch reads it before each
//! claim, and a trip pauses the campaign in Postgres.

use ad_events::CampaignEvent;

/// What the binary must do in response to an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactorAction {
    /// Claim the due leads up to the free capacity, and dial them.
    Dispatch {
        campaign_id: i64,
    },
    /// Update the lead from the ended attempt, then dispatch into the freed
    /// capacity.
    SettleAndDispatch {
        campaign_id: i64,
        attempt_id: i64,
    },
    Noop,
}

/// Decide the action for an event. Pure.
pub fn react(event: &CampaignEvent) -> ReactorAction {
    match event {
        CampaignEvent::CampaignRunning { campaign_id }
        | CampaignEvent::AgentJoined { campaign_id, .. } => ReactorAction::Dispatch {
            campaign_id: *campaign_id,
        },
        CampaignEvent::AttemptEnded {
            campaign_id,
            attempt_id,
        } => ReactorAction::SettleAndDispatch {
            campaign_id: *campaign_id,
            attempt_id: *attempt_id,
        },
        CampaignEvent::CallCompleted { .. }
        | CampaignEvent::RetryScheduled { .. }
        | CampaignEvent::CircuitBreakerTripped { .. }
        | CampaignEvent::CampaignCompleted { .. } => ReactorAction::Noop,
    }
}

#[cfg(test)]
#[path = "reactor_tests.rs"]
mod reactor_tests;
