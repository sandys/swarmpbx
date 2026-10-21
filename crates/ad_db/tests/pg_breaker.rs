//! Integration tests for the Postgres circuit-breaker window: the breaker reads
//! the ended call attempts and replays them into the pure engine. Gated on
//! DATABASE_URL.

mod common;

use ad_attempt::{AttemptEvent, NotDialedReason};
use ad_campaign::Transition;
use ad_circuit_breaker::Config;
use ad_core::CampaignId;
use ad_db::Db;
use ad_events::BreakerStore;
use chrono::{DateTime, Duration, Utc};
use common::{apply_all, claim, complete, leads, reject, running_campaign};

/// End one attempt per entry, each on a new lead. "failed" is a SIP 503,
/// "no_answer" a SIP 480, "answered" a completed call. Times are `now -
/// secs_ago`.
async fn attempts(db: &Db, c: CampaignId, now: DateTime<Utc>, entries: &[(&str, i64)]) {
    leads(db, c, entries.len()).await;
    let claimed = claim(db, c, entries.len() as i64).await;
    assert_eq!(claimed.len(), entries.len());
    for (a, (outcome, secs_ago)) in claimed.iter().zip(entries) {
        match *outcome {
            "failed" => reject(db, a.attempt_id, 503, now, *secs_ago).await,
            "no_answer" => reject(db, a.attempt_id, 480, now, *secs_ago).await,
            "answered" => complete(db, a.attempt_id, now, *secs_ago).await,
            other => panic!("unknown outcome {other}"),
        }
    }
}

#[tokio::test]
async fn failed_attempts_in_the_window_open_the_breaker() {
    let Some(db) = common::db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let c = running_campaign(&db, 100).await;
    let now = Utc::now();
    let cfg = Config::default(); // 0.5 / 120 s / min 5
    attempts(
        &db,
        c,
        now,
        &[
            ("failed", 10),
            ("failed", 9),
            ("failed", 8),
            ("failed", 7),
            ("answered", 6),
            ("failed", 5),
        ],
    )
    .await;

    let (open, stats) = db.breaker().evaluate(c.0, now, &cfg).await.unwrap();
    assert!(open, "5 of 6 failed: {stats:?}");
    assert_eq!(stats.total, 6);
    assert_eq!(stats.failures, 5);
}

#[tokio::test]
async fn attempts_outside_the_window_and_non_failures_do_not_count() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 100).await;
    let now = Utc::now();
    let cfg = Config::default();
    // Old failures (outside 120 s) and no_answer (not a failure) only.
    attempts(
        &db,
        c,
        now,
        &[
            ("failed", 500),
            ("failed", 400),
            ("failed", 300),
            ("failed", 200),
            ("failed", 121),
            ("no_answer", 3),
            ("no_answer", 2),
            ("no_answer", 1),
            ("no_answer", 0),
            ("no_answer", 0),
        ],
    )
    .await;

    let (open, stats) = db.breaker().evaluate(c.0, now, &cfg).await.unwrap();
    assert!(!open, "{stats:?}");
    assert_eq!(stats.total, 5, "only the in-window attempts count");
    assert_eq!(stats.failures, 0);
}

#[tokio::test]
async fn an_attempt_that_never_dialed_does_not_count() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 100).await;
    let now = Utc::now();
    leads(&db, c, 6).await;
    for a in claim(&db, c, 6).await {
        apply_all(
            &db,
            a.attempt_id,
            &[AttemptEvent::NotDialed {
                at: now,
                reason: NotDialedReason::Window,
                detail: "outside calling window".into(),
            }],
        )
        .await;
    }
    let (open, stats) = db
        .breaker()
        .evaluate(c.0, now, &Config::default())
        .await
        .unwrap();
    assert!(!open);
    assert_eq!(stats.total, 0);
}

#[tokio::test]
async fn a_resume_resets_the_breaker() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = running_campaign(&db, 100).await;
    let now = Utc::now();
    let fails: Vec<(&str, i64)> = (0..6).map(|i| ("failed", 10 + i)).collect();
    attempts(&db, c, now, &fails).await;
    let cfg = Config::default();
    assert!(db.breaker().evaluate(c.0, now, &cfg).await.unwrap().0);

    // An operator pauses and resumes: the old failures stop counting.
    for t in [Transition::Pause, Transition::Resume] {
        db.campaigns().transition(c, t, now).await.unwrap().unwrap();
    }
    let (open, stats) = db.breaker().evaluate(c.0, now, &cfg).await.unwrap();
    assert!(!open, "{stats:?}");
    assert_eq!(stats.total, 0);

    // A failure after the resume counts again.
    let later = now + Duration::seconds(1);
    attempts(&db, c, later, &[("failed", 0)]).await;
    assert_eq!(
        db.breaker()
            .evaluate(c.0, later, &cfg)
            .await
            .unwrap()
            .1
            .total,
        1
    );
}

#[tokio::test]
async fn the_breaker_is_per_campaign() {
    let Some(db) = common::db().await else {
        return;
    };
    let bad = running_campaign(&db, 100).await;
    let good = running_campaign(&db, 100).await;
    let now = Utc::now();
    let cfg = Config::default();
    let fails: Vec<(&str, i64)> = (0..6).map(|i| ("failed", i)).collect();
    attempts(&db, bad, now, &fails).await;

    assert!(db.breaker().evaluate(bad.0, now, &cfg).await.unwrap().0);
    assert!(!db.breaker().evaluate(good.0, now, &cfg).await.unwrap().0);
}
