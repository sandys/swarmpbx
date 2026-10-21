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

fn every_variant(campaign_id: i64) -> Vec<CampaignEvent> {
    vec![
        CampaignEvent::CampaignRunning { campaign_id },
        CampaignEvent::AttemptEnded {
            campaign_id,
            attempt_id: 11,
        },
        CampaignEvent::CallCompleted {
            campaign_id,
            lead_id: 1,
            answered: true,
        },
        CampaignEvent::RetryScheduled {
            campaign_id,
            lead_id: 1,
            due_epoch_ms: 0,
        },
        CampaignEvent::CircuitBreakerTripped {
            campaign_id,
            failure_rate: 0.5,
        },
        CampaignEvent::CampaignCompleted { campaign_id },
    ]
}

#[test]
fn event_round_trips_through_json() {
    for e in every_variant(1) {
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
    for e in every_variant(3) {
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

#[test]
fn kind_matches_the_serde_type_tag_for_every_variant() {
    for e in every_variant(1) {
        let v: serde_json::Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!(v["type"], e.kind(), "kind() drifted from the serde tag");
    }
}

#[test]
fn retry_reason_storage_string_matches_serde() {
    for r in [
        RetryReason::Busy,
        RetryReason::NoAnswer,
        RetryReason::Voicemail,
        RetryReason::Failed,
        RetryReason::Error,
    ] {
        assert_eq!(serde_json::to_value(r).unwrap(), r.as_str());
    }
}
