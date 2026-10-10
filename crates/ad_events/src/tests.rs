use super::*;

#[test]
fn event_json_wire_shape_matches_tagged_contract() {
    let e = CampaignEvent::CallCompleted {
        campaign_id: 7,
        lead_id: 42,
        answered: false,
    };
    let json = e.to_json();
    // The `type` tag and fields must be flat (dograh-compatible wire shape).
    assert!(json.contains("\"type\":\"call_completed\""), "got: {json}");
    assert!(json.contains("\"campaign_id\":7"));
    assert!(json.contains("\"lead_id\":42"));
    assert!(json.contains("\"answered\":false"));
}

#[test]
fn event_round_trips_through_json() {
    let events = vec![
        CampaignEvent::BatchCompleted {
            campaign_id: 1,
            processed_count: 10,
            failed_count: 2,
            batch_size: 10,
        },
        CampaignEvent::RetryNeeded {
            campaign_id: 1,
            lead_id: 5,
            reason: RetryReason::NoAnswer,
        },
        CampaignEvent::CircuitBreakerTripped {
            campaign_id: 1,
            failure_rate: 0.8,
        },
        CampaignEvent::CampaignCompleted { campaign_id: 1 },
    ];
    for e in events {
        let back = CampaignEvent::from_json(&e.to_json()).unwrap();
        assert_eq!(back, e);
    }
}

#[test]
fn bad_json_is_a_decode_error_not_a_panic() {
    assert!(matches!(
        CampaignEvent::from_json("not json"),
        Err(EventError::Decode(_))
    ));
    assert!(matches!(
        CampaignEvent::from_json("{\"type\":\"nope\"}"),
        Err(EventError::Decode(_))
    ));
}

#[test]
fn campaign_id_accessor_covers_every_variant() {
    let all = [
        CampaignEvent::BatchCompleted {
            campaign_id: 3,
            processed_count: 0,
            failed_count: 0,
            batch_size: 0,
        },
        CampaignEvent::BatchFailed {
            campaign_id: 3,
            error: "x".into(),
            processed_count: 0,
        },
        CampaignEvent::CallCompleted {
            campaign_id: 3,
            lead_id: 1,
            answered: true,
        },
        CampaignEvent::RetryNeeded {
            campaign_id: 3,
            lead_id: 1,
            reason: RetryReason::Busy,
        },
        CampaignEvent::RetryScheduled {
            campaign_id: 3,
            lead_id: 1,
            due_epoch_ms: 0,
        },
        CampaignEvent::CircuitBreakerTripped {
            campaign_id: 3,
            failure_rate: 0.5,
        },
        CampaignEvent::CampaignCompleted { campaign_id: 3 },
    ];
    for e in all {
        assert_eq!(e.campaign_id(), 3);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn in_memory_bus_is_fifo_publish_then_consume() {
    let bus = InMemoryBus::new();
    let mut consumer = bus.clone();
    bus.publish(&CampaignEvent::CampaignCompleted { campaign_id: 1 })
        .await
        .unwrap();
    bus.publish(&CampaignEvent::CallCompleted {
        campaign_id: 1,
        lead_id: 9,
        answered: true,
    })
    .await
    .unwrap();
    assert_eq!(bus.pending(), 2);

    let first = consumer.next_event().await.unwrap().unwrap();
    assert_eq!(first, CampaignEvent::CampaignCompleted { campaign_id: 1 });
    let second = consumer.next_event().await.unwrap().unwrap();
    assert!(matches!(
        second,
        CampaignEvent::CallCompleted { lead_id: 9, .. }
    ));
    assert!(consumer.next_event().await.unwrap().is_none(), "drained");
}

#[tokio::test(flavor = "current_thread")]
async fn due_timers_pop_only_leads_at_or_before_now_for_the_campaign() {
    let t = InMemoryTimers::new();
    t.schedule(1, 100, 1_000).await.unwrap(); // campaign 1, lead 100, due 1000
    t.schedule(1, 101, 5_000).await.unwrap(); // not yet due at now=2000
    t.schedule(2, 200, 1_000).await.unwrap(); // other campaign

    let due = t.pop_due(1, 2_000).await.unwrap();
    assert_eq!(due, vec![100], "only campaign-1 leads due by now");

    // Popped entries are removed; the not-yet-due one survives.
    let again = t.pop_due(1, 2_000).await.unwrap();
    assert!(again.is_empty(), "already popped");

    // The future one becomes due later.
    let later = t.pop_due(1, 6_000).await.unwrap();
    assert_eq!(later, vec![101]);
}
