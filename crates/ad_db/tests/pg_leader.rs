//! Integration tests for the advisory-lock leader and the concurrency counter.
//! Gated on DATABASE_URL.

use std::collections::BTreeMap;

use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_core::{CampaignId, OrgId};
use ad_db::leader::{in_call_count, try_become_leader};
use ad_db::Db;
use ad_lead::repo::{LeadRepo, NewLead};

async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

fn unique_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64)
        & 0x7fff_ffff
}

#[tokio::test]
async fn advisory_lock_is_exclusive_then_released() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let key = unique_i64();

    // First caller becomes leader.
    let guard = try_become_leader(db.pool(), key).await.unwrap();
    assert!(guard.is_some(), "first acquire should win");

    // While held, a second acquire fails.
    let second = try_become_leader(db.pool(), key).await.unwrap();
    assert!(
        second.is_none(),
        "second acquire must fail while lock is held"
    );

    // Release and re-acquire.
    drop(guard);
    // Give the pool a moment to return the connection and release the session lock.
    let mut reacquired = None;
    for _ in 0..50 {
        if let Some(g) = try_become_leader(db.pool(), key).await.unwrap() {
            reacquired = Some(g);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        reacquired.is_some(),
        "lock should be re-acquirable after the guard drops"
    );
}

#[tokio::test]
async fn in_call_count_reflects_claimed_leads() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let org = OrgId(unique_i64());
    let campaign = db
        .campaigns()
        .create(NewCampaign {
            org_id: org,
            name: "cap".into(),
        })
        .await
        .unwrap()
        .id;

    // Seed 5 leads into the hopper.
    let news: Vec<NewLead> = (0..5)
        .map(|i| NewLead {
            campaign_id: campaign,
            phone_number: format!("+1555{i:07}"),
            timezone: Some("America/New_York".into()),
            state: None,
            postal_code: None,
            context: BTreeMap::new(),
        })
        .collect();
    let leads = db.leads().bulk_create(news).await.unwrap();
    let ids: Vec<_> = leads.iter().map(|l| l.id).collect();
    db.hopper().insert_leads(campaign, &ids, 0).await.unwrap();

    assert_eq!(in_call_count(db.pool(), campaign).await.unwrap(), 0);

    // Claim 2 -> they become in-call (claimed).
    let claimed = db.hopper().claim_batch(campaign, "w", 2).await.unwrap();
    assert_eq!(claimed.len(), 2);
    assert_eq!(in_call_count(db.pool(), campaign).await.unwrap(), 2);

    // A different campaign is unaffected.
    let other = db
        .campaigns()
        .create(NewCampaign {
            org_id: org,
            name: "other".into(),
        })
        .await
        .unwrap()
        .id;
    assert_eq!(
        in_call_count(db.pool(), CampaignId(other.0)).await.unwrap(),
        0
    );
}
