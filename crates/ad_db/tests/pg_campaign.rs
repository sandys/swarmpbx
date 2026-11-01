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

use ad_campaign::repo::{CampaignChange, CampaignRepo, NewCampaign, RepoError};
use ad_campaign::{CampaignState, Retry, Transition};
use ad_core::{CampaignId, OrgId};
use ad_db::Db;
use ad_events::CampaignEvent;

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
            concurrency_cap: 25,
            agent: None,
            retry: None,
        })
        .await
        .unwrap();
    assert_eq!(created.state, CampaignState::Draft);
    assert_eq!(created.org_id, org);
    assert_eq!(created.concurrency_cap, 25);

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
        concurrency_cap: 10,
        agent: None,
        retry: None,
    })
    .await
    .unwrap();
    repo.create(NewCampaign {
        org_id: org_a,
        name: "a2".into(),
        concurrency_cap: 10,
        agent: None,
        retry: None,
    })
    .await
    .unwrap();
    repo.create(NewCampaign {
        org_id: org_b,
        name: "b1".into(),
        concurrency_cap: 10,
        agent: None,
        retry: None,
    })
    .await
    .unwrap();

    assert_eq!(repo.list(org_a).await.unwrap().len(), 2);
    assert_eq!(repo.list(org_b).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_update_changes_only_the_given_settings() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.campaigns();
    let agent = serde_json::json!({"runtime": "livekit", "agent_name": "a"});
    let c = repo
        .create(NewCampaign {
            org_id: unique_org(),
            name: "before".into(),
            concurrency_cap: 5,
            agent: Some(agent.clone()),
            retry: None,
        })
        .await
        .unwrap();
    for t in [Transition::Start, Transition::SyncComplete] {
        repo.transition(c.id, t, chrono::Utc::now())
            .await
            .unwrap()
            .unwrap();
    }
    // The orchestrator completes the campaign after the caller read it.
    assert!(repo.complete_if_idle(c.id).await.unwrap());

    let now = chrono::Utc::now();
    let cap = repo
        .update(
            c.id,
            CampaignChange {
                concurrency_cap: Some(7),
                ..CampaignChange::default()
            },
            now,
        )
        .await
        .unwrap();
    assert_eq!(
        cap.state,
        CampaignState::Completed,
        "the update wrote the state"
    );
    assert_eq!(
        (cap.name.as_str(), cap.concurrency_cap, cap.agent.as_ref()),
        ("before", 7, Some(&agent))
    );

    let other = serde_json::json!({"runtime": "pipecat", "start_url": "http://bot/start"});
    let renamed = repo
        .update(
            c.id,
            CampaignChange {
                name: Some("after".into()),
                agent: Some(other.clone()),
                ..CampaignChange::default()
            },
            now,
        )
        .await
        .unwrap();
    assert_eq!(
        (
            renamed.name.as_str(),
            renamed.concurrency_cap,
            renamed.agent.as_ref()
        ),
        ("after", 7, Some(&other))
    );
    assert_eq!(repo.get(c.id).await.unwrap(), renamed);

    let missing = repo
        .update(CampaignId(i64::MAX), CampaignChange::default(), now)
        .await;
    assert!(matches!(missing, Err(RepoError::NotFound(_))));

    // Each change tells the leader, so it dispatches with the new values.
    let changed = db
        .events()
        .page(c.id.0, 0, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|(_, _, e)| matches!(e, CampaignEvent::SettingsChanged { .. }))
        .count();
    assert_eq!(changed, 2);
}

#[tokio::test]
async fn retry_settings_merge_by_key_and_keep_their_defaults() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let repo = db.campaigns();
    let c = repo
        .create(NewCampaign {
            org_id: unique_org(),
            name: "retry".into(),
            concurrency_cap: 1,
            agent: None,
            retry: Some(serde_json::json!({"busy_s": 30})),
        })
        .await
        .unwrap();
    assert_eq!(
        c.retry,
        Retry {
            busy_s: 30,
            ..Retry::default()
        }
    );
    let now = chrono::Utc::now();
    let change = |r: serde_json::Value| CampaignChange {
        retry: Some(r),
        ..CampaignChange::default()
    };
    repo.update(c.id, change(serde_json::json!({"max_calls": 5})), now)
        .await
        .unwrap();
    let both = repo
        .update(c.id, change(serde_json::json!({"no_answer_s": 40})), now)
        .await
        .unwrap();
    assert_eq!(
        both.retry,
        Retry {
            busy_s: 30,
            max_calls: 5,
            no_answer_s: 40,
            ..Retry::default()
        }
    );
    assert_eq!(repo.get(c.id).await.unwrap().retry, both.retry);

    let refused = repo
        .update(c.id, change(serde_json::json!({"max_calls": 0})), now)
        .await;
    assert!(matches!(refused, Err(RepoError::Storage(_))));
    assert_eq!(repo.get(c.id).await.unwrap().retry, both.retry);
}
