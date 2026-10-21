//! Shared setup for the Postgres integration tests. Each test makes its own
//! campaign, so the tests run in parallel on one database.

#![allow(dead_code)]

use std::collections::BTreeMap;

use ad_attempt::AttemptEvent;
use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_campaign::Transition;
use ad_core::{AttemptId, CampaignId, OrgId};
use ad_db::{ClaimedAttempt, Db};
use ad_lead::repo::{LeadRepo, NewLead};
use ad_lead::Lead;
use chrono::{DateTime, Duration, Utc};

pub async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

pub fn unique_org() -> OrgId {
    use std::time::{SystemTime, UNIX_EPOCH};
    OrgId(
        (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64)
            & 0x7fff_ffff,
    )
}

/// A fixed time for tests that set every event time themselves.
pub fn t0() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

/// A campaign in the `draft` state.
pub async fn campaign(db: &Db, cap: u32) -> CampaignId {
    db.campaigns()
        .create(NewCampaign {
            org_id: unique_org(),
            name: "test".into(),
            concurrency_cap: cap,
            agent: None,
        })
        .await
        .unwrap()
        .id
}

/// A campaign in the `running` state.
pub async fn running_campaign(db: &Db, cap: u32) -> CampaignId {
    let id = campaign(db, cap).await;
    for t in [Transition::Start, Transition::SyncComplete] {
        db.campaigns()
            .transition(id, t, Utc::now())
            .await
            .unwrap()
            .unwrap();
    }
    id
}

pub async fn leads(db: &Db, campaign_id: CampaignId, n: usize) -> Vec<Lead> {
    let news = (0..n)
        .map(|i| NewLead {
            campaign_id,
            phone_number: format!("+1555{:03}{i:04}", campaign_id.0 % 1000),
            timezone: Some("America/New_York".into()),
            state: None,
            postal_code: None,
            context: BTreeMap::new(),
        })
        .collect();
    db.leads().bulk_create(news).await.unwrap()
}

pub async fn claim(db: &Db, campaign_id: CampaignId, limit: i64) -> Vec<ClaimedAttempt> {
    db.attempts()
        .claim(campaign_id, "test-worker", limit, "room", Utc::now())
        .await
        .unwrap()
}

/// Apply events that must all change the attempt.
pub async fn apply_all(db: &Db, attempt_id: AttemptId, events: &[AttemptEvent]) {
    for e in events {
        let applied = db.attempts().apply(attempt_id, e).await.unwrap();
        assert!(
            matches!(applied, ad_db::Applied::Changed(_)),
            "{} did not apply: {applied:?}",
            e.kind()
        );
    }
}

/// Dial and end an attempt with a SIP rejection `secs_ago` before `now`.
pub async fn reject(
    db: &Db,
    attempt_id: AttemptId,
    sip_code: u16,
    now: DateTime<Utc>,
    secs_ago: i64,
) {
    let at = now - Duration::seconds(secs_ago);
    apply_all(
        db,
        attempt_id,
        &[
            AttemptEvent::DialStarted { at },
            AttemptEvent::Rejected {
                at,
                sip_code: Some(sip_code),
                detail: format!("SIP {sip_code}"),
            },
        ],
    )
    .await;
}

/// Dial, answer, and hang up an attempt `secs_ago` before `now`.
pub async fn complete(db: &Db, attempt_id: AttemptId, now: DateTime<Utc>, secs_ago: i64) {
    let at = now - Duration::seconds(secs_ago);
    apply_all(
        db,
        attempt_id,
        &[
            AttemptEvent::DialStarted { at },
            AttemptEvent::Answered { at },
            AttemptEvent::Hangup {
                at,
                detail: "bye".into(),
            },
        ],
    )
    .await;
}
