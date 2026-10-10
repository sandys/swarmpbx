use super::*;
use ad_events::{CampaignEvent, RetryReason};

fn input(breaker_open: bool, attempts: u32) -> ReactorInput {
    ReactorInput {
        now_epoch_ms: 1_000_000,
        breaker_open,
        lead_attempts: attempts,
        policy: RetryPolicy::default(),
    }
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
        react(&e, &input(false, 0)),
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
        react(&e, &input(true, 0)),
        ReactorAction::PauseCampaign { campaign_id: 7 }
    );
}

#[test]
fn retry_needed_schedules_at_policy_interval() {
    let e = CampaignEvent::RetryNeeded {
        campaign_id: 7,
        lead_id: 42,
        reason: RetryReason::NoAnswer,
    };
    let got = react(&e, &input(false, 0));
    // now (1_000_000) + no_answer interval (30 min = 1_800_000 ms)
    assert_eq!(
        got,
        ReactorAction::ScheduleRetry {
            campaign_id: 7,
            lead_id: 42,
            due_epoch_ms: 1_000_000 + 1_800_000
        }
    );
}

#[test]
fn retry_reason_changes_the_interval() {
    let busy = CampaignEvent::RetryNeeded {
        campaign_id: 1,
        lead_id: 1,
        reason: RetryReason::Busy,
    };
    let vm = CampaignEvent::RetryNeeded {
        campaign_id: 1,
        lead_id: 1,
        reason: RetryReason::Voicemail,
    };
    let busy_due = match react(&busy, &input(false, 0)) {
        ReactorAction::ScheduleRetry { due_epoch_ms, .. } => due_epoch_ms,
        other => panic!("{other:?}"),
    };
    let vm_due = match react(&vm, &input(false, 0)) {
        ReactorAction::ScheduleRetry { due_epoch_ms, .. } => due_epoch_ms,
        other => panic!("{other:?}"),
    };
    assert!(vm_due > busy_due, "voicemail backs off longer than busy");
}

#[test]
fn retry_dropped_at_max_attempts() {
    let e = CampaignEvent::RetryNeeded {
        campaign_id: 7,
        lead_id: 42,
        reason: RetryReason::Failed,
    };
    // default max_attempts = 3; at 3 attempts, drop.
    assert_eq!(
        react(&e, &input(false, 3)),
        ReactorAction::DropRetry {
            campaign_id: 7,
            lead_id: 42
        }
    );
}

#[test]
fn circuit_breaker_tripped_pauses() {
    let e = CampaignEvent::CircuitBreakerTripped {
        campaign_id: 7,
        failure_rate: 0.9,
    };
    assert_eq!(
        react(&e, &input(false, 0)),
        ReactorAction::PauseCampaign { campaign_id: 7 }
    );
}

#[test]
fn campaign_completed_stops() {
    let e = CampaignEvent::CampaignCompleted { campaign_id: 7 };
    assert_eq!(
        react(&e, &input(false, 0)),
        ReactorAction::StopCampaign { campaign_id: 7 }
    );
}

#[test]
fn informational_events_are_noop() {
    let scheduled = CampaignEvent::RetryScheduled {
        campaign_id: 1,
        lead_id: 1,
        due_epoch_ms: 0,
    };
    let completed = CampaignEvent::CallCompleted {
        campaign_id: 1,
        lead_id: 1,
        answered: true,
    };
    assert_eq!(react(&scheduled, &input(false, 0)), ReactorAction::Noop);
    assert_eq!(react(&completed, &input(false, 0)), ReactorAction::Noop);
}
