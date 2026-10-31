//! HTTP contract tests for the LiveKit webhook endpoint. The tests that end a
//! call attempt need Postgres (DATABASE_URL); they skip without it.

use std::collections::BTreeMap;
use std::sync::Arc;

use ad_api::{router, AppState, CampaignStore, LeadStore, Webhooks};
use ad_attempt::{AttemptEvent, Outcome};
use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_campaign::Transition;
use ad_core::time::SystemClock;
use ad_core::OrgId;
use ad_db::{ClaimedAttempt, Db, PgAttemptRepo};
use ad_gateway::WebhookVerifier;
use ad_lead::repo::{LeadRepo, NewLead};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use chrono::Utc;
use http_body_util::BodyExt;
use livekit_api::access_token::AccessToken;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const KEY: &str = "devkey";
const SECRET: &str = "devsecret_devsecret_devsecret_32b";

/// Sign a body the way LiveKit does: a JWT that holds the body's SHA-256.
fn sign(body: &str) -> String {
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
    AccessToken::with_api_key(KEY, SECRET)
        .with_sha256(&hash)
        .to_jwt()
        .unwrap()
}

async fn post(app: axum::Router, body: &str, auth: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/webhooks/livekit")
        .header("content-type", "application/webhook+json");
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let resp = app
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn left(room: &str, identity: &str) -> String {
    json!({
        "event": "participant_left",
        "room": { "name": room },
        "participant": { "identity": identity },
        "id": "EV_test",
    })
    .to_string()
}

fn app_with(attempts: PgAttemptRepo, db: Option<&Db>) -> axum::Router {
    let mut state = match db {
        Some(db) => AppState::with_stores(
            Arc::new(SystemClock),
            CampaignStore::Pg(db.campaigns()),
            LeadStore::Pg(db.leads()),
        ),
        None => AppState::new(Arc::new(SystemClock)),
    };
    state.webhooks = Some(Webhooks {
        verifier: WebhookVerifier::new(KEY, SECRET, Arc::new(SystemClock)),
        attempts,
    });
    router(state)
}

/// A repository that never connects: for tests that stop before storage.
fn unconnected() -> PgAttemptRepo {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
        .unwrap();
    PgAttemptRepo::new(pool)
}

#[tokio::test]
async fn without_keys_and_postgres_the_endpoint_is_off() {
    let app = router(AppState::new(Arc::new(SystemClock)));
    let body = left("r", "i");
    let (status, v) = post(app, &body, Some(&sign(&body))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(v["error"].as_str().unwrap().contains("not configured"));
}

#[tokio::test]
async fn a_missing_or_wrong_signature_is_refused() {
    let body = left("r", "i");
    let (status, _) = post(app_with(unconnected(), None), &body, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let other = left("r", "someone-else");
    let (status, _) = post(app_with(unconnected(), None), &body, Some(&sign(&other))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn other_events_end_nothing() {
    let body = json!({"event": "track_published", "room": {"name": "r"}, "id": "EV_x"}).to_string();
    let (status, v) = post(app_with(unconnected(), None), &body, Some(&sign(&body))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["ended"], 0);
}

async fn db() -> Option<Db> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

/// A running campaign with `n` leads, all claimed and answered.
async fn connected_calls(db: &Db, n: usize) -> Vec<ClaimedAttempt> {
    calls(db, n, None).await
}

/// As `connected_calls`; with an `agent`, the agent `agent-{attempt id}` is in
/// the room before the dial.
async fn calls(db: &Db, n: usize, agent: Option<Value>) -> Vec<ClaimedAttempt> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let org = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64)
        & 0x7fff_ffff;
    let c = db
        .campaigns()
        .create(NewCampaign {
            org_id: OrgId(org),
            name: "webhook".into(),
            concurrency_cap: 10,
            agent: agent.clone(),
        })
        .await
        .unwrap()
        .id;
    for t in [Transition::Start, Transition::SyncComplete] {
        db.campaigns()
            .transition(c, t, Utc::now())
            .await
            .unwrap()
            .unwrap();
    }
    let news = (0..n)
        .map(|i| NewLead {
            campaign_id: c,
            phone_number: format!("+1555777{i:04}"),
            timezone: Some("America/New_York".into()),
            state: None,
            postal_code: None,
            context: BTreeMap::new(),
        })
        .collect();
    db.leads().bulk_create(news).await.unwrap();
    let claimed = db
        .attempts()
        .claim(c, "test", n as i64, &format!("wh{org}"), Utc::now(), None)
        .await
        .unwrap();
    for a in &claimed {
        let mut events = Vec::new();
        if agent.is_some() {
            events.push(AttemptEvent::AgentStarting {
                at: Utc::now(),
                detail: "dispatch".into(),
            });
            events.push(AttemptEvent::AgentReady {
                at: Utc::now(),
                identity: format!("agent-{}", a.attempt_id.0),
            });
        }
        events.push(AttemptEvent::DialStarted { at: Utc::now() });
        events.push(AttemptEvent::Answered { at: Utc::now() });
        for e in events {
            db.attempts().apply(a.attempt_id, &e).await.unwrap();
        }
    }
    claimed
}

#[tokio::test]
async fn participant_left_ends_that_call_once() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let a = connected_calls(&db, 1).await.remove(0);
    let app = || app_with(db.attempts(), Some(&db));

    // Another participant leaves the room: the call goes on.
    let agent = left(&a.room, "agent-1");
    let (status, v) = post(app(), &agent, Some(&sign(&agent))).await;
    assert_eq!((status, v["ended"].clone()), (StatusCode::OK, json!(0)));

    let body = left(&a.room, &a.participant_identity);
    let (status, v) = post(app(), &body, Some(&sign(&body))).await;
    assert_eq!((status, v["ended"].clone()), (StatusCode::OK, json!(1)));
    let snapshot = db.attempts().snapshot(a.attempt_id).await.unwrap();
    assert!(matches!(
        snapshot.outcome(),
        Some(Outcome::Completed { .. })
    ));

    // LiveKit delivers the same webhook again: nothing changes.
    let (status, v) = post(app(), &body, Some(&sign(&body))).await;
    assert_eq!((status, v["ended"].clone()), (StatusCode::OK, json!(0)));
}

#[tokio::test]
async fn room_finished_ends_every_call_in_the_room() {
    let Some(db) = db().await else {
        return;
    };
    let calls = connected_calls(&db, 2).await;
    // Both leads of this test share a room prefix but not a room. Close the
    // first room only.
    let body = json!({"event": "room_finished", "room": {"name": calls[0].room}, "id": "EV_f"})
        .to_string();
    let (status, v) = post(
        app_with(db.attempts(), Some(&db)),
        &body,
        Some(&sign(&body)),
    )
    .await;
    assert_eq!((status, v["ended"].clone()), (StatusCode::OK, json!(1)));
    assert!(db
        .attempts()
        .snapshot(calls[1].attempt_id)
        .await
        .unwrap()
        .leg
        .is_live());
}

#[tokio::test]
async fn the_agent_leaving_ends_the_call() {
    let Some(db) = db().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let agent = json!({"runtime": "livekit", "agent_name": "sales"});
    let a = calls(&db, 1, Some(agent)).await.remove(0);
    let app = || app_with(db.attempts(), Some(&db));

    // A participant that is not the agent leaves: the call goes on.
    let other = left(&a.room, "agent-0");
    let (_, v) = post(app(), &other, Some(&sign(&other))).await;
    assert_eq!(v["ended"], json!(0));

    let body = left(&a.room, &format!("agent-{}", a.attempt_id.0));
    let (status, v) = post(app(), &body, Some(&sign(&body))).await;
    assert_eq!((status, v["ended"].clone()), (StatusCode::OK, json!(1)));
    let snapshot = db.attempts().snapshot(a.attempt_id).await.unwrap();
    assert_eq!(snapshot.agent.name(), "left");
    assert!(matches!(
        snapshot.outcome(),
        Some(Outcome::Completed { .. })
    ));
}
