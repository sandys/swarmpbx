//! Integration tests for the Postgres circuit-breaker window: the breaker reads
//! `call_outcomes` and replays it into the pure engine. Gated on DATABASE_URL.

use std::collections::BTreeMap;

use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_circuit_breaker::Config;
use ad_core::{CampaignId, OrgId};
use ad_db::{Db, OutcomeRecord};
use ad_events::BreakerStore;
use ad_lead::repo::{LeadRepo, NewLead};
use ad_lead::LeadStatus;
use chrono::{DateTime, Duration, Utc};

async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

async fn campaign(db: &Db) -> CampaignId {
    use std::time::{SystemTime, UNIX_EPOCH};
    let org = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64)
        & 0x7fff_ffff;
    db.campaigns()
        .create(NewCampaign {
            org_id: OrgId(org),
            name: "breaker-test".into(),
        })
        .await
        .unwrap()
        .id
}

/// Record one outcome per entry, each on a new lead, at `now - secs_ago`.
async fn outcomes(db: &Db, c: CampaignId, now: DateTime<Utc>, entries: &[(&str, i64)]) {
    for (i, (outcome, secs_ago)) in entries.iter().enumerate() {
        let lead = db
            .leads()
            .create(NewLead {
                campaign_id: c,
                phone_number: format!("+1555100{i:04}"),
                timezone: None,
                state: None,
                postal_code: None,
                context: BTreeMap::new(),
            })
            .await
            .unwrap();
        db.leads()
            .record_outcome(&OutcomeRecord {
                campaign_id: c,
                lead_id: lead.id,
                outcome,
                answered: *outcome == "answered",
                attempt_at: now - Duration::seconds(*secs_ago),
                next_status: LeadStatus::Dispositioned,
                retry: None,
            })
            .await
            .unwrap();
    }
}

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

#[tokio::test]
async fn failed_outcomes_in_the_window_open_the_breaker() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let c = campaign(&db).await;
    let cfg = Config::default(); // 0.5 / 120 s / min 5
    outcomes(
        &db,
        c,
        now(),
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

    let (open, stats) = db.breaker().evaluate(c.0, now(), &cfg).await.unwrap();
    assert!(open, "5 of 6 failed: {stats:?}");
    assert_eq!(stats.total, 6);
    assert_eq!(stats.failures, 5);
}

#[tokio::test]
async fn outcomes_outside_the_window_and_non_failures_do_not_count() {
    let Some(db) = db().await else {
        return;
    };
    let c = campaign(&db).await;
    let cfg = Config::default();
    // Old failures (outside 120 s) and no_answer (not a failure) only.
    outcomes(
        &db,
        c,
        now(),
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

    let (open, stats) = db.breaker().evaluate(c.0, now(), &cfg).await.unwrap();
    assert!(!open, "{stats:?}");
    assert_eq!(stats.total, 5, "only the in-window rows count");
    assert_eq!(stats.failures, 0);
}

#[tokio::test]
async fn the_breaker_is_per_campaign() {
    let Some(db) = db().await else {
        return;
    };
    let bad = campaign(&db).await;
    let good = campaign(&db).await;
    let cfg = Config::default();
    let fails: Vec<(&str, i64)> = (0..6).map(|i| ("failed", i)).collect();
    outcomes(&db, bad, now(), &fails).await;

    assert!(db.breaker().evaluate(bad.0, now(), &cfg).await.unwrap().0);
    assert!(!db.breaker().evaluate(good.0, now(), &cfg).await.unwrap().0);
}
