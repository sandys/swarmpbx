//! Integration test for the Postgres hopper claim. Proves SELECT ... FOR UPDATE
//! SKIP LOCKED gives concurrent workers disjoint, non-overlapping leads.
//! Gated on DATABASE_URL.

use std::collections::BTreeMap;
use std::collections::HashSet;

use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_core::{CampaignId, LeadId, OrgId};
use ad_db::{Db, OutcomeRecord, ScheduledRetry};
use ad_lead::repo::{LeadRepo, NewLead};
use ad_lead::LeadStatus;
use chrono::{DateTime, Utc};

async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

fn unique_org() -> OrgId {
    use std::time::{SystemTime, UNIX_EPOCH};
    OrgId(
        (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64)
            & 0x7fff_ffff,
    )
}

async fn seed(db: &Db, n: usize) -> (CampaignId, Vec<LeadId>) {
    let campaign = db
        .campaigns()
        .create(NewCampaign {
            org_id: unique_org(),
            name: "hopper-test".into(),
        })
        .await
        .unwrap()
        .id;
    let news: Vec<NewLead> = (0..n)
        .map(|i| NewLead {
            campaign_id: campaign,
            phone_number: format!("+1555000{i:04}"),
            timezone: Some("America/New_York".into()),
            state: None,
            postal_code: None,
            context: BTreeMap::new(),
        })
        .collect();
    let leads = db.leads().bulk_create(news).await.unwrap();
    let ids: Vec<LeadId> = leads.iter().map(|l| l.id).collect();
    db.hopper().insert_leads(campaign, &ids, 0).await.unwrap();
    (campaign, ids)
}

#[tokio::test]
async fn concurrent_claims_are_disjoint() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (campaign, ids) = seed(&db, 50).await;
    assert_eq!(db.hopper().available_count(campaign).await.unwrap(), 50);

    // Two workers claim concurrently, in batches, until the hopper drains.
    async fn drain(db: Db, campaign: CampaignId, worker: &str) -> Vec<i64> {
        let repo = db.hopper();
        let mut got = Vec::new();
        loop {
            let batch = repo.claim_batch(campaign, worker, 7).await.unwrap();
            if batch.is_empty() {
                break;
            }
            got.extend(batch.into_iter().map(|c| c.lead_id.0));
        }
        got
    }

    let (a, b) = tokio::join!(
        drain(db.clone(), campaign, "worker-a"),
        drain(db.clone(), campaign, "worker-b"),
    );

    // No lead claimed by both workers.
    let set_a: HashSet<i64> = a.iter().copied().collect();
    let set_b: HashSet<i64> = b.iter().copied().collect();
    assert!(
        set_a.is_disjoint(&set_b),
        "workers claimed overlapping leads"
    );

    // Every seeded lead was claimed exactly once, in total.
    let mut all: Vec<i64> = a.iter().chain(b.iter()).copied().collect();
    all.sort_unstable();
    let mut expected: Vec<i64> = ids.iter().map(|l| l.0).collect();
    expected.sort_unstable();
    assert_eq!(all, expected, "every lead claimed exactly once");

    // Hopper fully drained.
    assert_eq!(db.hopper().available_count(campaign).await.unwrap(), 0);
}

#[tokio::test]
async fn rolled_back_claim_releases_lock_for_recovery() {
    // Crash recovery: SKIP LOCKED means an in-flight claim holds a row lock
    // that other workers skip; if that worker "crashes" (its transaction rolls
    // back), the lock is released and the lead becomes claimable again. We
    // simulate the crash by holding a FOR UPDATE SKIP LOCKED lock in one
    // transaction, then rolling it back.
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (campaign, ids) = seed(&db, 1).await; // exactly one hopper item
    let only_lead = ids[0].0;

    // Transaction A locks the single row (worker mid-claim), does NOT commit.
    let mut tx_a = db.pool().begin().await.unwrap();
    let locked: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM hopper WHERE campaign_id = $1 AND claimed_by IS NULL \
         ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED",
    )
    .bind(campaign.0)
    .fetch_optional(&mut *tx_a)
    .await
    .unwrap();
    assert!(locked.is_some(), "A should have locked the row");

    // While A holds the lock, a normal claim by B must find nothing (SKIP
    // LOCKED skips the locked row rather than blocking on it).
    let b_before = db
        .hopper()
        .claim_batch(campaign, "worker-b", 10)
        .await
        .unwrap();
    assert!(b_before.is_empty(), "B must skip the row A has locked");

    // A "crashes": roll back. The lock releases and the row is available again.
    tx_a.rollback().await.unwrap();

    // Now B claims it successfully — recovery without any manual cleanup.
    let b_after = db
        .hopper()
        .claim_batch(campaign, "worker-b", 10)
        .await
        .unwrap();
    assert_eq!(b_after.len(), 1);
    assert_eq!(b_after[0].lead_id.0, only_lead);
}

/// Record an attempt that schedules a retry at `due_at`.
async fn schedule_retry(db: &Db, campaign: CampaignId, lead: LeadId, due_at: DateTime<Utc>) {
    let attempt_at = due_at - chrono::Duration::minutes(30);
    db.leads()
        .record_outcome(&OutcomeRecord {
            campaign_id: campaign,
            lead_id: lead,
            outcome: "no_answer",
            answered: false,
            attempt_at,
            next_status: LeadStatus::Queued,
            retry: Some(ScheduledRetry {
                due_at,
                reason: "no_answer",
            }),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn readmit_due_returns_only_due_leads_and_makes_them_claimable() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let (campaign, ids) = seed(&db, 3).await;
    let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();

    // Dial all three: the hopper rows are claimed.
    let claimed = db.hopper().claim_batch(campaign, "w", 10).await.unwrap();
    assert_eq!(claimed.len(), 3);
    assert_eq!(db.hopper().available_count(campaign).await.unwrap(), 0);

    // Lead 0 is due; lead 1 is due later; lead 2 has no hopper row any more.
    schedule_retry(&db, campaign, ids[0], now - chrono::Duration::seconds(1)).await;
    schedule_retry(&db, campaign, ids[1], now + chrono::Duration::minutes(5)).await;
    schedule_retry(&db, campaign, ids[2], now).await;
    sqlx::query("DELETE FROM hopper WHERE lead_id = $1")
        .bind(ids[2].0)
        .execute(db.pool())
        .await
        .unwrap();

    let mut due = db.hopper().readmit_due(campaign, now).await.unwrap();
    due.sort_by_key(|l| l.0);
    assert_eq!(due, vec![ids[0], ids[2]], "due at or before now only");

    // The due leads are claimable again; the future one is not.
    assert_eq!(db.hopper().available_count(campaign).await.unwrap(), 2);
    let next_eligible: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT next_eligible_at FROM leads WHERE id = $1")
            .bind(ids[0].0)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(next_eligible, None, "the due time is cleared");

    // A second sweep finds nothing, so a repeated sweep is harmless.
    assert!(db
        .hopper()
        .readmit_due(campaign, now)
        .await
        .unwrap()
        .is_empty());

    // The later lead becomes due later.
    let later = now + chrono::Duration::minutes(6);
    assert_eq!(
        db.hopper().readmit_due(campaign, later).await.unwrap(),
        vec![ids[1]]
    );
}

#[tokio::test]
async fn readmit_due_ignores_leads_that_are_not_queued() {
    let Some(db) = db().await else {
        return;
    };
    let (campaign, ids) = seed(&db, 1).await;
    let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
    schedule_retry(&db, campaign, ids[0], now - chrono::Duration::seconds(1)).await;
    // The lead left the queue (for example DNC) after the retry was set.
    sqlx::query("UPDATE leads SET status = 'dnc' WHERE id = $1")
        .bind(ids[0].0)
        .execute(db.pool())
        .await
        .unwrap();

    assert!(db
        .hopper()
        .readmit_due(campaign, now)
        .await
        .unwrap()
        .is_empty());
}
