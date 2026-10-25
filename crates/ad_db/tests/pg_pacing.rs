//! Integration tests for join-paced dialing on Postgres (GAPS.md DIAL-9): the
//! window of an agent pool in the claim, the learning from samples, and the
//! settings that change at run time. Gated on DATABASE_URL.

mod common;

use ad_attempt::AttemptEvent;
use ad_core::{AttemptId, CampaignId};
use ad_db::{ClaimRequest, ClaimedAttempt, Db, Listed, PaceClaim};
use ad_pacing::{Mode, Settings};
use chrono::{DateTime, Duration, Utc};
use common::{apply_all, leads, running_campaign};

async fn agent_campaign(db: &Db, cap: u32) -> CampaignId {
    let c = running_campaign(db, cap).await;
    sqlx::query("UPDATE campaigns SET agent = $2 WHERE id = $1")
        .bind(c.0)
        .bind(serde_json::json!({"runtime": "livekit", "agent_name": "sales"}))
        .execute(db.pool())
        .await
        .unwrap();
    c
}

fn pace(pool: &str, mode: Mode) -> PaceClaim {
    PaceClaim {
        pool: pool.into(),
        mode,
        connect_within_ms: 2000,
    }
}

/// Claim customer first within the pool's window.
async fn claim(db: &Db, c: CampaignId, p: &PaceClaim) -> Vec<ClaimedAttempt> {
    db.attempts()
        .claim_paced(
            c,
            &ClaimRequest {
                worker: "test-worker",
                limit: 50,
                room_prefix: "room",
                now: Utc::now(),
                agent_on_answer: Some(2000),
                pace: Some(p),
            },
        )
        .await
        .unwrap()
}

async fn first_k(db: &Db, pool: &str, k: u32) {
    let s = Settings {
        first_k: Some(k),
        ..Settings::default()
    };
    db.pacing().set_settings(pool, &s, true).await.unwrap();
}

/// Answer at `at`, and start the agent.
async fn answer(db: &Db, a: AttemptId, at: DateTime<Utc>) {
    apply_all(
        db,
        a,
        &[
            AttemptEvent::DialStarted { at },
            AttemptEvent::Answered { at },
            AttemptEvent::AgentStarting {
                at,
                detail: "dispatch".into(),
            },
        ],
    )
    .await;
}

async fn ready(db: &Db, a: AttemptId, at: DateTime<Utc>) {
    apply_all(
        db,
        a,
        &[AttemptEvent::AgentReady {
            at,
            identity: format!("agent-{}", a.0),
        }],
    )
    .await;
}

async fn busy(db: &Db, a: AttemptId, at: DateTime<Utc>) {
    common::reject(db, a, 486, at, 0).await;
}

fn pool_name(c: CampaignId) -> String {
    format!("livekit:test-pool-{}", c.0)
}

#[tokio::test]
async fn the_window_counts_the_attempts_whose_agent_is_not_in_the_room() {
    let Some(db) = common::db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 6).await;
    let pool = pool_name(c);
    first_k(&db, &pool, 3).await;
    let p = pace(&pool, Mode::Adaptive);

    let first = claim(&db, c, &p).await;
    assert_eq!(first.len(), 3, "k = 3, below the campaign cap");
    let rows: Vec<(String, i32, i32)> = sqlx::query_as(
        "SELECT pool, pool_window, pool_limit FROM call_attempts WHERE campaign_id = $1 ORDER BY id",
    )
    .bind(c.0)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        rows,
        [1, 2, 3].map(|w| (pool.clone(), w, 3)),
        "each attempt keeps its window and limit"
    );
    assert!(claim(&db, c, &p).await.is_empty(), "the window is full");

    // A ringing or answered call stays in the window until its agent joins.
    let now = Utc::now();
    answer(&db, first[0].attempt_id, now).await;
    assert!(claim(&db, c, &p).await.is_empty(), "answered, no agent yet");
    ready(&db, first[0].attempt_id, now + Duration::milliseconds(300)).await;
    assert_eq!(claim(&db, c, &p).await.len(), 1, "the agent joined");

    // An ended call leaves the window.
    busy(&db, first[1].attempt_id, now).await;
    assert_eq!(claim(&db, c, &p).await.len(), 1, "the call ended");
    assert!(claim(&db, c, &p).await.is_empty());
}

#[tokio::test]
async fn two_campaigns_of_one_pool_share_one_window() {
    let Some(db) = common::db().await else {
        return;
    };
    let c1 = agent_campaign(&db, 10).await;
    let c2 = agent_campaign(&db, 10).await;
    leads(&db, c1, 3).await;
    leads(&db, c2, 3).await;
    let pool = pool_name(c1);
    first_k(&db, &pool, 2).await;
    let p = pace(&pool, Mode::Adaptive);
    assert_eq!(claim(&db, c1, &p).await.len(), 2);
    assert!(claim(&db, c2, &p).await.is_empty(), "the pool is full");
    let mut tasks = Vec::new();
    for c in [c1, c2] {
        let (db, p) = (db.clone(), p.clone());
        tasks.push(tokio::spawn(async move { claim(&db, c, &p).await.len() }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 0);
    }
}

#[tokio::test]
async fn the_pool_learns_from_each_sample_once_and_keeps_the_state() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 3).await;
    let pool = pool_name(c);
    first_k(&db, &pool, 1).await;
    let p = pace(&pool, Mode::Adaptive);

    let a = claim(&db, c, &p).await;
    assert_eq!(a.len(), 1, "slow start begins at 1");
    let now = Utc::now();
    answer(&db, a[0].attempt_id, now).await;
    ready(&db, a[0].attempt_id, now + Duration::milliseconds(400)).await;
    let s = db.pacing().ingest(&pool, 2000).await.unwrap();
    assert_eq!((s.k, s.samples), (2, 1), "one fast join doubles k");
    let s = db.pacing().ingest(&pool, 2000).await.unwrap();
    assert_eq!((s.k, s.samples), (2, 1), "a sample teaches once");

    let view = db.pacing().view(&pool, 2000).await.unwrap();
    assert_eq!(view.state.k, 2, "a new reader sees the saved state");
    assert_eq!(view.recent.joins, 1);
    assert_eq!(view.recent.p50_ms, Some(400));
    assert_eq!(view.limits.target_ms, 1000);

    // A join above the back-off halves k and ends slow start.
    let b = claim(&db, c, &p).await;
    assert_eq!(b.len(), 2);
    let now = Utc::now();
    answer(&db, b[0].attempt_id, now).await;
    ready(&db, b[0].attempt_id, now + Duration::milliseconds(1600)).await;
    busy(&db, b[1].attempt_id, now).await;
    let s = db.pacing().ingest(&pool, 2000).await.unwrap();
    assert_eq!((s.k, s.slow, s.slow_start), (1, 1, false));
    assert_eq!(s.samples, 2, "a busy call says nothing about the pool");
}

#[tokio::test]
async fn an_abandoned_call_halves_k_once_for_its_round() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 4).await;
    let pool = pool_name(c);
    first_k(&db, &pool, 4).await;
    let p = pace(&pool, Mode::Adaptive);
    let burst = claim(&db, c, &p).await;
    assert_eq!(burst.len(), 4);
    let now = Utc::now();
    for (i, a) in burst.iter().enumerate() {
        answer(&db, a.attempt_id, now).await;
        apply_all(
            &db,
            a.attempt_id,
            &[AttemptEvent::AgentFailed {
                at: now + Duration::milliseconds(2000 + i as i64),
                detail: "no agent in the room 2000 ms after the answer".into(),
            }],
        )
        .await;
    }
    let s = db.pacing().ingest(&pool, 2000).await.unwrap();
    assert_eq!(s.k, 2, "four abandoned calls of one burst halve k once");
    assert_eq!(s.slow, 4);
}

#[tokio::test]
async fn the_proven_number_counts_the_starts_at_once() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 3).await;
    let pool = pool_name(c);
    first_k(&db, &pool, 3).await;
    let p = pace(&pool, Mode::Adaptive);
    let burst = claim(&db, c, &p).await;
    let now = Utc::now();
    for a in &burst {
        answer(&db, a.attempt_id, now).await;
    }
    for (i, a) in burst.iter().enumerate() {
        ready(
            &db,
            a.attempt_id,
            now + Duration::milliseconds(300 + 50 * i as i64),
        )
        .await;
    }
    let s = db.pacing().ingest(&pool, 2000).await.unwrap();
    assert_eq!(
        s.proven, 3,
        "three agents started at once, and all joined fast"
    );
}

#[tokio::test]
async fn strict_allows_one_start_more_than_the_pool_proved() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 4).await;
    let pool = pool_name(c);
    first_k(&db, &pool, 4).await;
    assert_eq!(db.pacing().mode(c).await.unwrap(), Some(Mode::Adaptive));
    assert!(db.pacing().set_mode(c, Mode::Strict).await.unwrap());
    let mode = db.pacing().mode(c).await.unwrap().unwrap();
    assert_eq!(mode, Mode::Strict);
    assert_eq!(
        claim(&db, c, &pace(&pool, mode)).await.len(),
        1,
        "the pool proved nothing yet"
    );
    assert!(!db
        .pacing()
        .set_mode(CampaignId(i64::MAX), Mode::Strict)
        .await
        .unwrap());
    assert_eq!(db.pacing().mode(CampaignId(i64::MAX)).await.unwrap(), None);
}

#[tokio::test]
async fn settings_that_do_not_read_fail_closed_to_one() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 3).await;
    let pool = pool_name(c);
    sqlx::query("INSERT INTO agent_pools (pool, settings) VALUES ($1, '{\"target_ms\": 0}')")
        .bind(&pool)
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(claim(&db, c, &pace(&pool, Mode::Adaptive)).await.len(), 1);
    assert!(db.pacing().ingest(&pool, 2000).await.is_err());
    let listed = db.pacing().list(2000).await.unwrap();
    assert!(
        listed
            .iter()
            .any(|l| matches!(l, Listed::Invalid { pool: p, .. } if *p == pool)),
        "the list shows the pool with its error"
    );
}

#[tokio::test]
async fn a_reset_starts_the_pool_again_at_its_first_k() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 1).await;
    let pool = pool_name(c);
    first_k(&db, &pool, 1).await;
    let p = pace(&pool, Mode::Adaptive);
    let a = claim(&db, c, &p).await;
    let now = Utc::now();
    answer(&db, a[0].attempt_id, now).await;
    ready(&db, a[0].attempt_id, now + Duration::milliseconds(200)).await;
    assert_eq!(db.pacing().ingest(&pool, 2000).await.unwrap().k, 2);
    let s = Settings {
        first_k: Some(5),
        ..Settings::default()
    };
    db.pacing().set_settings(&pool, &s, false).await.unwrap();
    assert_eq!(
        db.pacing().view(&pool, 2000).await.unwrap().state.k,
        2,
        "no reset"
    );
    db.pacing().set_settings(&pool, &s, true).await.unwrap();
    let v = db.pacing().view(&pool, 2000).await.unwrap();
    assert_eq!((v.state.k, v.state.slow_start), (5, true));
    let listed = db.pacing().list(2000).await.unwrap();
    assert!(listed
        .iter()
        .any(|l| matches!(l, Listed::Pool(v) if v.pool == pool)));
}

#[tokio::test]
async fn only_a_missing_agent_at_the_limit_is_a_failed_join() {
    let Some(db) = common::db().await else {
        return;
    };
    let c = agent_campaign(&db, 10).await;
    leads(&db, c, 2).await;
    let pool = pool_name(c);
    first_k(&db, &pool, 2).await;
    let p = pace(&pool, Mode::Adaptive);
    let burst = claim(&db, c, &p).await;
    let now = Utc::now();
    for a in &burst {
        answer(&db, a.attempt_id, now).await;
    }
    // The node stopped 300 ms after the answer: abandoned, but not slow.
    apply_all(
        &db,
        burst[0].attempt_id,
        &[AttemptEvent::Absent {
            at: now + Duration::milliseconds(300),
        }],
    )
    .await;
    let s = db.pacing().ingest(&pool, 2000).await.unwrap();
    assert_eq!((s.k, s.samples, s.slow), (2, 0, 0), "the pool was not slow");
    // The callee hung up 2.5 s after the answer, with no agent yet.
    apply_all(
        &db,
        burst[1].attempt_id,
        &[AttemptEvent::Hangup {
            at: now + Duration::milliseconds(2500),
            detail: "the callee hung up".into(),
        }],
    )
    .await;
    let s = db.pacing().ingest(&pool, 2000).await.unwrap();
    assert_eq!((s.k, s.slow), (1, 1), "no agent by the limit halves k");
}
