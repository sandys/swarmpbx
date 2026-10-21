//! Integration tests for the Postgres lead repository. Gated on DATABASE_URL;
//! skips cleanly when unset. Creates a parent campaign first (FK), then leads.

use std::collections::BTreeMap;

use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_core::{CampaignId, LeadId, OrgId};
use ad_db::{Db, OutcomeRecord};
use ad_lead::repo::{LeadRepo, NewLead, RepoError};

async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect to test postgres");
    db.migrate().await.expect("run migrations");
    Some(db)
}

fn unique_org() -> OrgId {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    OrgId((nanos as i64) & 0x7fff_ffff)
}

async fn make_campaign(db: &Db) -> CampaignId {
    db.campaigns()
        .create(NewCampaign {
            org_id: unique_org(),
            name: "lead-test".into(),
        })
        .await
        .unwrap()
        .id
}

fn nl(campaign: CampaignId, phone: &str) -> NewLead {
    let mut ctx = BTreeMap::new();
    ctx.insert("customer_name".to_string(), "Jane".to_string());
    NewLead {
        campaign_id: campaign,
        phone_number: phone.into(),
        timezone: Some("America/New_York".into()),
        state: Some("NY".into()),
        postal_code: Some("10001".into()),
        context: ctx,
    }
}

#[tokio::test]
async fn create_get_roundtrip_with_context() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let campaign = make_campaign(&db).await;
    let repo = db.leads();

    let created = repo.create(nl(campaign, "+13125550100")).await.unwrap();
    assert_eq!(created.campaign_id, campaign);
    assert_eq!(
        created.context.get("customer_name").map(String::as_str),
        Some("Jane")
    );

    let got = repo.get(created.id).await.unwrap();
    assert_eq!(got, created);
}

#[tokio::test]
async fn bulk_create_and_list_scoped_per_campaign() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.leads();
    let c1 = make_campaign(&db).await;
    let c2 = make_campaign(&db).await;

    repo.bulk_create(vec![nl(c1, "+1a"), nl(c1, "+1b"), nl(c1, "+1c")])
        .await
        .unwrap();
    repo.bulk_create(vec![nl(c2, "+2a")]).await.unwrap();

    assert_eq!(repo.list_by_campaign(c1).await.unwrap().len(), 3);
    assert_eq!(repo.list_by_campaign(c2).await.unwrap().len(), 1);
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.leads();
    assert!(matches!(
        repo.get(LeadId(-1)).await,
        Err(RepoError::NotFound(LeadId(-1)))
    ));
}

#[tokio::test]
async fn unknown_status_in_db_fails_closed() {
    // Fail-closed on bad data: a row whose status string the engine does not
    // recognize must surface a Storage error, never a silent default. We write
    // an invalid status directly (bypassing the typed API) and read it back.
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let campaign = make_campaign(&db).await;
    let lead = db
        .leads()
        .create(nl(campaign, "+15550009999"))
        .await
        .unwrap();

    sqlx::query("UPDATE leads SET status = 'bogus_status' WHERE id = $1")
        .bind(lead.id.0)
        .execute(db.pool())
        .await
        .unwrap();

    match db.leads().get(lead.id).await {
        Err(RepoError::Storage(msg)) => assert!(msg.contains("unknown lead status")),
        other => panic!("expected Storage error on unknown status, got {other:?}"),
    }
}

#[tokio::test]
async fn record_outcome_is_idempotent_on_replay() {
    let Some(db) = db().await else {
        return;
    };
    let campaign = make_campaign(&db).await;
    let lead = db
        .leads()
        .create(nl(campaign, "+15550007777"))
        .await
        .unwrap();
    assert_eq!(lead.called_count, 0);

    let attempt = chrono::DateTime::from_timestamp(1_700_000_500, 0).unwrap();

    let rec = OutcomeRecord {
        campaign_id: campaign,
        lead_id: lead.id,
        outcome: "no_answer",
        answered: false,
        attempt_at: attempt,
        next_status: ad_lead::LeadStatus::Queued,
        retry: None,
    };

    // First record: new -> true, lead advances once.
    let first = db.leads().record_outcome(&rec).await.unwrap();
    assert!(first, "first record is new");

    // Replay the SAME attempt (event delivery is at-least-once): no-op -> false.
    let replay = db.leads().record_outcome(&rec).await.unwrap();
    assert!(!replay, "replay of same attempt is a no-op");

    // called_count advanced exactly once, status set, last call time stamped.
    let after = db.leads().get(lead.id).await.unwrap();
    assert_eq!(after.called_count, 1, "no double count on replay");
    assert_eq!(after.status, ad_lead::LeadStatus::Queued);
    assert_eq!(after.last_local_call_time, Some(attempt));

    // Only one outcome row exists for this lead.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM call_outcomes WHERE lead_id = $1")
        .bind(lead.id.0)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn record_outcome_persists_and_clears_the_retry() {
    let Some(db) = db().await else {
        return;
    };
    let campaign = make_campaign(&db).await;
    let lead = db
        .leads()
        .create(nl(campaign, "+15550006666"))
        .await
        .unwrap();
    let first = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let due = first + chrono::Duration::minutes(30);

    db.leads()
        .record_outcome(&OutcomeRecord {
            campaign_id: campaign,
            lead_id: lead.id,
            outcome: "no_answer",
            answered: false,
            attempt_at: first,
            next_status: ad_lead::LeadStatus::Queued,
            retry: Some(ad_db::ScheduledRetry {
                due_at: due,
                reason: "no_answer",
            }),
        })
        .await
        .unwrap();
    let row: (Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
        sqlx::query_as("SELECT next_eligible_at, retry_reason FROM leads WHERE id = $1")
            .bind(lead.id.0)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(row, (Some(due), Some("no_answer".to_string())));

    // A later answered attempt clears the retry.
    db.leads()
        .record_outcome(&OutcomeRecord {
            campaign_id: campaign,
            lead_id: lead.id,
            outcome: "answered",
            answered: true,
            attempt_at: due,
            next_status: ad_lead::LeadStatus::Dispositioned,
            retry: None,
        })
        .await
        .unwrap();
    let row: (Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
        sqlx::query_as("SELECT next_eligible_at, retry_reason FROM leads WHERE id = $1")
            .bind(lead.id.0)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(row, (None, None));
}
