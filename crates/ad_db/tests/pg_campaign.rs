//! Integration tests for the Postgres campaign repository.
//!
//! These run against a real Postgres. Start it first:
//!     docker compose up -d
//! Then run with the test database URL:
//!     DATABASE_URL=postgres://agentdialer:agentdialer@127.0.0.1:5433/agentdialer \
//!         cargo test -p ad_db -- --include-ignored
//!
//! Without `DATABASE_URL` the tests skip, so a normal `cargo test --all` on a
//! machine with no database stays green. Each test uses a unique org_id so runs
//! do not collide on shared data.

use ad_campaign::repo::{CampaignRepo, NewCampaign, RepoError};
use ad_campaign::{CampaignState, Transition};
use ad_core::{CampaignId, OrgId};
use ad_db::Db;

/// Connect + migrate, or return None when DATABASE_URL is unset (skip).
async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect to test postgres");
    db.migrate().await.expect("run migrations");
    Some(db)
}

/// A unique org id per test, so parallel/independent runs do not interfere.
fn unique_org() -> OrgId {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    OrgId((nanos as i64) & 0x7fff_ffff)
}

#[tokio::test]
async fn create_get_list_save_roundtrip() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.campaigns();
    let org = unique_org();

    let created = repo
        .create(NewCampaign {
            org_id: org,
            name: "pg camp".into(),
        })
        .await
        .unwrap();
    assert_eq!(created.state, CampaignState::Draft);
    assert_eq!(created.org_id, org);

    let got = repo.get(created.id).await.unwrap();
    assert_eq!(got, created);

    let listed = repo.list(org).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);

    // Mutate via the state machine, then persist.
    let mut c = got;
    c.apply(Transition::Start, chrono::Utc::now()).unwrap();
    let saved = repo.save(c).await.unwrap();
    assert_eq!(saved.state, CampaignState::Syncing);

    // Re-read confirms the state persisted.
    let reread = repo.get(created.id).await.unwrap();
    assert_eq!(reread.state, CampaignState::Syncing);
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.campaigns();
    let err = repo.get(CampaignId(-1)).await.unwrap_err();
    assert!(matches!(err, RepoError::NotFound(CampaignId(-1))));
}

#[tokio::test]
async fn save_missing_is_not_found() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.campaigns();
    let ghost =
        ad_campaign::Campaign::new(CampaignId(-2), OrgId(1), "ghost".into(), chrono::Utc::now());
    assert!(matches!(
        repo.save(ghost).await,
        Err(RepoError::NotFound(_))
    ));
}

#[tokio::test]
async fn list_is_scoped_per_org() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.campaigns();
    let org_a = unique_org();
    let org_b = unique_org();

    repo.create(NewCampaign {
        org_id: org_a,
        name: "a1".into(),
    })
    .await
    .unwrap();
    repo.create(NewCampaign {
        org_id: org_a,
        name: "a2".into(),
    })
    .await
    .unwrap();
    repo.create(NewCampaign {
        org_id: org_b,
        name: "b1".into(),
    })
    .await
    .unwrap();

    assert_eq!(repo.list(org_a).await.unwrap().len(), 2);
    assert_eq!(repo.list(org_b).await.unwrap().len(), 1);
}
