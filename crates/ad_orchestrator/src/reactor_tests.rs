use super::*;

#[test]
fn a_running_campaign_dispatches() {
    assert_eq!(
        react(&CampaignEvent::CampaignRunning { campaign_id: 4 }),
        ReactorAction::Dispatch { campaign_id: 4 }
    );
}

#[test]
fn an_ended_attempt_settles_then_dispatches() {
    // A finished call frees capacity, so it must start new dials (GAPS.md
    // ORCH-4).
    assert_eq!(
        react(&CampaignEvent::AttemptEnded {
            campaign_id: 4,
            attempt_id: 9
        }),
        ReactorAction::SettleAndDispatch {
            campaign_id: 4,
            attempt_id: 9
        }
    );
}

#[test]
fn an_agent_in_the_room_dispatches() {
    // The attempt left the pacing window of its pool (GAPS.md DIAL-9).
    assert_eq!(
        react(&CampaignEvent::AgentJoined {
            campaign_id: 4,
            attempt_id: 9
        }),
        ReactorAction::Dispatch { campaign_id: 4 }
    );
}

#[test]
fn an_agent_outcome_settles_its_attempt() {
    assert_eq!(
        react(&CampaignEvent::AgentOutcome {
            campaign_id: 4,
            attempt_id: 9,
            disposition: None,
            dnc: true,
        }),
        ReactorAction::SettleAndDispatch {
            campaign_id: 4,
            attempt_id: 9
        }
    );
}

#[test]
fn records_do_not_dispatch() {
    for e in [
        CampaignEvent::CallCompleted {
            campaign_id: 1,
            lead_id: 2,
            answered: true,
        },
        CampaignEvent::RetryScheduled {
            campaign_id: 1,
            lead_id: 2,
            due_epoch_ms: 0,
        },
        // The trip and the completion are already saved as the campaign state.
        // Reacting to them again would dial a paused or completed campaign.
        CampaignEvent::CircuitBreakerTripped {
            campaign_id: 1,
            failure_rate: 0.9,
        },
        CampaignEvent::AgentPoolTripped {
            campaign_id: 1,
            pool: "livekit:sales".into(),
            failed_in_row: 5,
            attempt_ids: vec![3],
        },
        CampaignEvent::CampaignCompleted { campaign_id: 1 },
    ] {
        assert_eq!(react(&e), ReactorAction::Noop, "{}", e.kind());
    }
}
