//! Integration tests for the cluster clock (`ad_db::clock`): the time of
//! Postgres plus the monotonic time. Gated on DATABASE_URL.

mod common;

use std::time::{Duration, Instant};

use ad_core::time::Clock;
use ad_db::clock::{PgClock, Read};
use chrono::{DateTime, Utc};

async fn db_now(db: &ad_db::Db) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_clock_tells_the_postgres_time() {
    let Some(db) = common::db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let clock = PgClock::start(db.pool().clone()).await;
    let before = clock.now_utc();
    let pg = db_now(&db).await;
    let after = clock.now_utc();
    assert!(
        before - chrono::Duration::milliseconds(100) <= pg
            && pg <= after + chrono::Duration::milliseconds(100),
        "Postgres said {pg}, the clock {before}..{after}"
    );
}

#[tokio::test]
async fn a_sync_corrects_a_clock_that_is_one_hour_off() {
    let Some(db) = common::db().await else {
        return;
    };
    let now = Instant::now();
    let off = Read {
        sent: now,
        received: now,
        db: db_now(&db).await - chrono::Duration::hours(1),
    };
    let clock = PgClock::at_anchor(db.pool().clone(), off);
    let gap = db_now(&db).await - clock.now_utc();
    assert!(
        gap > chrono::Duration::minutes(59),
        "the clock starts 1 h behind"
    );

    let synced = clock.sync().await.unwrap();
    assert!(
        (3_599_000..3_601_000).contains(&synced.step_ms),
        "step {} ms",
        synced.step_ms
    );
    assert!(synced.rtt < Duration::from_millis(500));
    let gap = db_now(&db).await - clock.now_utc();
    assert!(
        gap.num_milliseconds().abs() < 100,
        "after the sync, the clock is {} ms from Postgres",
        gap.num_milliseconds()
    );
}
