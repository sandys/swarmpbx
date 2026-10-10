//! Redis transport integration tests. Gated on `REDIS_URL`; skip when unset so
//! the fast tier stays hermetic. The Docker-first tier sets REDIS_URL to a real
//! Redis service.
#![cfg(feature = "redis")]

use ad_circuit_breaker::Config;
use ad_events::{BreakerStore, RedisBreakerStore};
use ad_events::{
    CampaignEvent, Consumer, DueTimers, Publisher, RedisConsumer, RedisPublisher, RedisTimers,
};

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

fn t(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
}

#[tokio::test]
async fn pub_sub_round_trip_over_real_redis() {
    let Some(url) = redis_url() else {
        eprintln!("REDIS_URL unset; skipping");
        return;
    };

    // The consumer must be subscribed before the publish, or pub/sub drops it.
    let mut consumer = RedisConsumer::connect(&url).await.unwrap();
    let publisher = RedisPublisher::connect(&url).await.unwrap();

    let sent = CampaignEvent::CallCompleted {
        campaign_id: 11,
        lead_id: 22,
        answered: false,
    };

    // Publish after a tiny spawn so the consumer is parked on next_event.
    let p = publisher.clone();
    let s = sent.clone();
    let handle = tokio::spawn(async move {
        // Retry briefly: the SUBSCRIBE may not be fully registered yet.
        for _ in 0..50 {
            p.publish(&s).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    let got = tokio::time::timeout(std::time::Duration::from_secs(10), consumer.next_event())
        .await
        .expect("event within 10s")
        .unwrap()
        .expect("some event");
    handle.abort();
    assert_eq!(got, sent);
}

#[tokio::test]
async fn due_timers_schedule_and_pop_over_real_redis() {
    let Some(url) = redis_url() else {
        eprintln!("REDIS_URL unset; skipping");
        return;
    };
    let timers = RedisTimers::connect(&url).await.unwrap();

    // Unique campaign id per run to avoid cross-test key collisions.
    let cid = 900_000 + (std::process::id() as i64 % 1000);

    timers.schedule(cid, 1, 1_000).await.unwrap();
    timers.schedule(cid, 2, 10_000).await.unwrap();

    let due_now = timers.pop_due(cid, 5_000).await.unwrap();
    assert_eq!(due_now, vec![1], "only lead 1 is due at t=5000");

    // Idempotent: popping again returns nothing (the window was removed).
    assert!(timers.pop_due(cid, 5_000).await.unwrap().is_empty());

    let later = timers.pop_due(cid, 20_000).await.unwrap();
    assert_eq!(later, vec![2]);
}

#[tokio::test]
async fn breaker_store_persists_window_and_trips_over_real_redis() {
    let Some(url) = redis_url() else {
        eprintln!("REDIS_URL unset; skipping");
        return;
    };
    let store = RedisBreakerStore::connect(&url).await.unwrap();
    let cfg = Config::default(); // 0.5 / 120s / min 5
    let cid = 800_000 + (std::process::id() as i64 % 1000);

    // Record failures in SEPARATE calls — the window must survive between them
    // (this is the whole point: state persisted in Redis, not reset each tick).
    for i in 0..4 {
        store.record(cid, t(i), true).await.unwrap();
    }
    assert!(
        !store.evaluate(cid, t(4), &cfg).await.unwrap().0,
        "below min_calls"
    );

    store.record(cid, t(5), true).await.unwrap();
    store.record(cid, t(6), true).await.unwrap();
    let (open, stats) = store.evaluate(cid, t(6), &cfg).await.unwrap();
    assert!(open, "6 persisted failures -> breaker open");
    assert_eq!(stats.total, 6);
}
