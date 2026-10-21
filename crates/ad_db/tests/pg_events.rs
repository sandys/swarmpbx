//! Integration tests for the Postgres event layer (an event table plus
//! LISTEN/NOTIFY). Gated on DATABASE_URL.
//!
//! The tests in this file run in parallel and share the event table, so each
//! test uses its own campaign and ignores the events of other campaigns.

use std::time::Duration;

use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_core::OrgId;
use ad_db::{Db, PgEventConsumer};
use ad_events::{CampaignEvent, Consumer, EventError, Publisher};

/// The deadline for one expected event. It is much shorter than the long
/// sweep, so a test that uses the long sweep proves the NOTIFY path.
const DEADLINE: Duration = Duration::from_secs(10);
const LONG_SWEEP: Duration = Duration::from_secs(3600);

async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

async fn campaign(db: &Db) -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let org = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64)
        & 0x7fff_ffff;
    db.campaigns()
        .create(NewCampaign {
            org_id: OrgId(org),
            name: "events-test".into(),
        })
        .await
        .unwrap()
        .id
        .0
}

/// The next event of `campaign_id`. Skips other campaigns' events and their
/// undecodable rows. Panics at the deadline.
async fn next_for(consumer: &mut PgEventConsumer, campaign_id: i64) -> CampaignEvent {
    tokio::time::timeout(DEADLINE, async {
        loop {
            match consumer.next_event().await {
                Ok(Some(e)) if e.campaign_id() == campaign_id => return e,
                Ok(_) | Err(EventError::Decode(_)) => continue,
                Err(e) => panic!("consumer error: {e}"),
            }
        }
    })
    .await
    .expect("event before the deadline")
}

fn completed(campaign_id: i64, lead_id: i64) -> CampaignEvent {
    CampaignEvent::CallCompleted {
        campaign_id,
        lead_id,
        answered: false,
    }
}

#[tokio::test]
async fn published_events_arrive_in_order_through_notify() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let c = campaign(&db).await;
    // The sweep is one hour: only a notification can wake the consumer in time.
    let mut consumer = PgEventConsumer::connect(db.pool(), LONG_SWEEP)
        .await
        .unwrap();

    let publisher = db.events();
    let waiter = tokio::spawn(async move {
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(next_for(&mut consumer, c).await);
        }
        got
    });
    for lead in 1..=3 {
        publisher.publish(&completed(c, lead)).await.unwrap();
    }

    let got = waiter.await.unwrap();
    assert_eq!(got, vec![completed(c, 1), completed(c, 2), completed(c, 3)]);
}

#[tokio::test]
async fn an_event_without_a_notification_is_found_by_the_sweep() {
    let Some(db) = db().await else {
        return;
    };
    let c = campaign(&db).await;
    let mut consumer = PgEventConsumer::connect(db.pool(), Duration::from_millis(200))
        .await
        .unwrap();

    // Write the row with no NOTIFY: this is a lost notification.
    let event = completed(c, 42);
    sqlx::query("INSERT INTO campaign_events (campaign_id, kind, payload) VALUES ($1, $2, $3)")
        .bind(c)
        .bind(event.kind())
        .bind(serde_json::to_value(&event).unwrap())
        .execute(db.pool())
        .await
        .unwrap();

    assert_eq!(next_for(&mut consumer, c).await, event);
}

#[tokio::test]
async fn delivery_starts_after_the_newest_event_at_connect() {
    let Some(db) = db().await else {
        return;
    };
    let c = campaign(&db).await;
    db.events().publish(&completed(c, 1)).await.unwrap();

    let mut consumer = PgEventConsumer::connect(db.pool(), Duration::from_millis(200))
        .await
        .unwrap();
    db.events().publish(&completed(c, 2)).await.unwrap();

    // Event 1 is older than the consumer and is not replayed.
    assert_eq!(next_for(&mut consumer, c).await, completed(c, 2));
}

#[tokio::test]
async fn an_undecodable_row_is_reported_once_and_skipped() {
    let Some(db) = db().await else {
        return;
    };
    let c = campaign(&db).await;
    let mut consumer = PgEventConsumer::connect(db.pool(), Duration::from_millis(200))
        .await
        .unwrap();

    let bad_id: i64 = sqlx::query_scalar(
        "INSERT INTO campaign_events (campaign_id, kind, payload) \
         VALUES ($1, 'unknown', '{\"type\":\"no_such_event\"}') RETURNING id",
    )
    .bind(c)
    .fetch_one(db.pool())
    .await
    .unwrap();
    db.events().publish(&completed(c, 7)).await.unwrap();

    // The bad row gives one Decode error that names it; the good event follows.
    let marker = format!("id {bad_id}:");
    let saw_error = tokio::time::timeout(DEADLINE, async {
        loop {
            match consumer.next_event().await {
                Err(EventError::Decode(m)) if m.contains(&marker) => return true,
                Ok(Some(e)) if e.campaign_id() == c => return false,
                Ok(_) | Err(EventError::Decode(_)) => continue,
                Err(e) => panic!("consumer error: {e}"),
            }
        }
    })
    .await
    .expect("result before the deadline");
    assert!(
        saw_error,
        "the bad row must be reported before the good event"
    );
    assert_eq!(next_for(&mut consumer, c).await, completed(c, 7));
}

#[tokio::test]
async fn the_kind_column_and_the_list_match_the_published_events() {
    let Some(db) = db().await else {
        return;
    };
    let c = campaign(&db).await;
    let log = db.events();
    let events = vec![
        completed(c, 1),
        CampaignEvent::BatchCompleted {
            campaign_id: c,
            processed_count: 1,
            failed_count: 0,
            batch_size: 1,
        },
        CampaignEvent::CampaignCompleted { campaign_id: c },
    ];
    for e in &events {
        log.publish(e).await.unwrap();
    }

    let listed: Vec<CampaignEvent> = log
        .list(c, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    assert_eq!(listed, events);

    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM campaign_events WHERE campaign_id = $1 ORDER BY id")
            .bind(c)
            .fetch_all(db.pool())
            .await
            .unwrap();
    let expected: Vec<&str> = events.iter().map(|e| e.kind()).collect();
    assert_eq!(kinds, expected);
}
