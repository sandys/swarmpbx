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
            retry: None,
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
    // Each load tells the leader, so it dials the new leads at once.
    for (c, count) in [(c1, 3), (c2, 1)] {
        let events: Vec<_> = db
            .events()
            .page(c.0, 0, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, _, e)| e)
            .collect();
        assert_eq!(
            events,
            [ad_events::CampaignEvent::LeadsAdded {
                campaign_id: c.0,
                count
            }]
        );
    }

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
async fn the_next_due_time_is_the_earliest_waiting_lead_of_a_running_campaign() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let now = chrono::Utc::now();
    let running = make_campaign(&db).await;
    for t in [
        ad_campaign::Transition::Start,
        ad_campaign::Transition::SyncComplete,
    ] {
        db.campaigns()
            .transition(running, t, now)
            .await
            .unwrap()
            .unwrap();
    }
    let draft = make_campaign(&db).await;
    let wait = |c: CampaignId, phone: &'static str, status: &'static str, after_s: i64| {
        let db = db.clone();
        async move {
            let lead = db.leads().create(nl(c, phone)).await.unwrap();
            sqlx::query("UPDATE leads SET status = $2, next_eligible_at = $3 WHERE id = $1")
                .bind(lead.id.0)
                .bind(status)
                .bind(now + chrono::Duration::seconds(after_s))
                .execute(db.pool())
                .await
                .unwrap();
        }
    };
    wait(running, "+1d1", "queued", 600).await;
    wait(running, "+1d2", "queued", 300).await;
    // Not waiting, or due already: the claim or nothing takes these.
    wait(running, "+1d3", "dnc", 60).await;
    wait(running, "+1d4", "queued", -60).await;
    wait(draft, "+1d5", "queued", 30).await;

    let due = db.leads().next_due(running, now).await.unwrap().unwrap();
    assert!(
        (due - (now + chrono::Duration::seconds(300)))
            .num_milliseconds()
            .abs()
            < 1
    );
    let all = db.leads().next_due_running(now).await.unwrap();
    assert!(all.contains(&(running, due)), "{all:?}");
    assert!(
        all.iter().all(|(c, _)| *c != draft),
        "a campaign that does not run has no due time"
    );
    assert_eq!(
        db.leads()
            .next_due(running, now + chrono::Duration::seconds(600))
            .await
            .unwrap(),
        None
    );
}
