use super::*;
use ad_events::{CampaignEvent, RetryReason};

fn input(breaker_open: bool) -> ReactorInput {
    ReactorInput { breaker_open }
}

#[test]
fn batch_completed_schedules_next_batch_when_breaker_closed() {
    let e = CampaignEvent::BatchCompleted {
        campaign_id: 7,
        processed_count: 10,
        failed_count: 1,
        batch_size: 10,
    };
    assert_eq!(
        react(&e, &input(false)),
        ReactorAction::ScheduleNextBatch { campaign_id: 7 }
    );
}

#[test]
fn batch_completed_pauses_when_breaker_open() {
    let e = CampaignEvent::BatchCompleted {
        campaign_id: 7,
        processed_count: 10,
        failed_count: 9,
        batch_size: 10,
    };
    assert_eq!(
        react(&e, &input(true)),
        ReactorAction::PauseCampaign { campaign_id: 7 }
    );
}

#[test]
fn retry_events_are_informational() {
    // The retry decision is persisted with the outcome (retry_decision), so the
    // reactor must not schedule anything from these events.
    let needed = CampaignEvent::RetryNeeded {
        campaign_id: 7,
        lead_id: 42,
        reason: RetryReason::NoAnswer,
    };
    let scheduled = CampaignEvent::RetryScheduled {
        campaign_id: 7,
        lead_id: 42,
        due_epoch_ms: 1,
    };
    assert_eq!(react(&needed, &input(false)), ReactorAction::Noop);
    assert_eq!(react(&scheduled, &input(false)), ReactorAction::Noop);
}

#[test]
fn circuit_breaker_tripped_pauses() {
    let e = CampaignEvent::CircuitBreakerTripped {
        campaign_id: 7,
        failure_rate: 0.9,
    };
    assert_eq!(
        react(&e, &input(false)),
        ReactorAction::PauseCampaign { campaign_id: 7 }
    );
}

#[test]
fn campaign_completed_stops() {
    let e = CampaignEvent::CampaignCompleted { campaign_id: 7 };
    assert_eq!(
        react(&e, &input(false)),
        ReactorAction::StopCampaign { campaign_id: 7 }
    );
}

#[test]
fn call_completed_is_noop() {
    let completed = CampaignEvent::CallCompleted {
        campaign_id: 1,
        lead_id: 1,
        answered: true,
    };
    assert_eq!(react(&completed, &input(false)), ReactorAction::Noop);
}
