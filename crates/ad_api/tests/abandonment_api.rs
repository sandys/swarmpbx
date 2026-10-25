//! HTTP contract test for the abandonment ledger of a campaign. It needs
//! Postgres (DATABASE_URL) and skips without it.

use std::collections::BTreeMap;
use std::sync::Arc;

use ad_api::{router, AppState, CampaignStore, LeadStore};
use ad_attempt::AttemptEvent;
use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_campaign::Transition;
use ad_core::time::SystemClock;
use ad_core::OrgId;
use ad_db::Db;
use ad_lead::repo::{LeadRepo, NewLead};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn get(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_ledger_reports_the_rate_and_stops_customer_first_above_the_cap() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    let mut state = AppState::with_stores(
        Arc::new(SystemClock),
        CampaignStore::Pg(db.campaigns()),
        LeadStore::Pg(db.leads()),
    );
    state.attempts = Some(db.attempts());
    let app = router(state);

    let org = (Utc::now().timestamp_nanos_opt().unwrap()) & 0x7fff_ffff;
    let c = db
        .campaigns()
        .create(NewCampaign {
            org_id: OrgId(org),
            name: "ledger".into(),
            concurrency_cap: 10,
            agent: Some(json!({"runtime": "livekit", "agent_name": "sales"})),
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
    let uri = format!("/v1/campaigns/{}/abandonment", c.0);
    let (status, v) = get(app.clone(), &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["rule"], "us_tsr");
    assert_eq!(v["window_days"], 30);
    assert_eq!(v["answered"], 0);
    assert_eq!(v["rate"], Value::Null);
    assert_eq!(v["connect_within_ms"], 2000);
    assert_eq!(v["dial_order"], "customer_first");
    assert_eq!(v["customer_first_allowed"], true);

    let news = (0..2)
        .map(|i| NewLead {
            campaign_id: c,
            phone_number: format!("+1555888{i:04}"),
            timezone: Some("America/New_York".into()),
            state: None,
            postal_code: None,
            context: BTreeMap::new(),
        })
        .collect();
    db.leads().bulk_create(news).await.unwrap();
    let claimed = db
        .attempts()
        .claim(c, "test", 2, &format!("ab{org}"), Utc::now(), Some(2000))
        .await
        .unwrap();
    let now = Utc::now();
    let ends = [
        AttemptEvent::Hangup {
            at: now,
            detail: "the callee hung up".into(),
        },
        AttemptEvent::AgentFailed {
            at: now,
            detail: "no agent in the room 2000 ms after the answer".into(),
        },
    ];
    for (a, end) in claimed.iter().zip(ends) {
        for e in [
            AttemptEvent::DialStarted { at: now },
            AttemptEvent::Answered { at: now },
            end,
        ] {
            db.attempts().apply(a.attempt_id, &e).await.unwrap();
        }
    }
    let (status, v) = get(app.clone(), &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (v["answered"].clone(), v["abandoned"].clone()),
        (json!(2), json!(1))
    );
    assert_eq!(v["rate"], 0.5);
    assert_eq!(
        v["customer_first_allowed"], false,
        "50% is above the 3% cap"
    );

    let (status, _) = get(app, "/v1/campaigns/999999999/abandonment").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
