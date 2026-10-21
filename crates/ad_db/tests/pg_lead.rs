//! Integration tests for the Postgres lead repository. Gated on DATABASE_URL;
//! skips cleanly when unset. Creates a parent campaign first (FK), then leads.

use std::collections::BTreeMap;

use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_core::{CampaignId, LeadId, OrgId};
use ad_db::Db;
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
            concurrency_cap: 10,
            agent: None,
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
