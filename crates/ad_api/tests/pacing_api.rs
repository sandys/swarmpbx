//! HTTP contract tests for the pacing of a campaign's agent pool (GAPS.md
//! DIAL-9). The values change at run time. The Postgres test skips without
//! DATABASE_URL.

use std::sync::Arc;

use ad_api::{router, AppState, CampaignStore, LeadStore};
use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_core::time::{FixedClock, SystemClock};
use ad_core::OrgId;
use ad_db::Db;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let request = if let Some(b) = body {
        builder = builder.header("content-type", "application/json");
        builder.body(Body::from(b.to_string())).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    };
    let resp = app.oneshot(request).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn pacing_needs_postgres() {
    let clock = FixedClock(ad_compliance::safe_hours::utc(2026, 6, 1, 12, 0));
    let app = router(AppState::new(Arc::new(clock)));
    for (method, uri, body) in [
        ("GET", "/v1/campaigns/1/pacing", None),
        (
            "PUT",
            "/v1/campaigns/1/pacing",
            Some(json!({"mode": "strict"})),
        ),
        ("GET", "/v1/agent-pools", None),
    ] {
        let (status, v) = send(app.clone(), method, uri, body).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{method} {uri}");
        assert!(v["error"].as_str().unwrap().contains("Postgres"));
    }
}

#[tokio::test]
async fn every_pacing_value_changes_at_run_time() {
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
    state.pacing = Some(db.pacing());
    let app = router(state);

    let org = Utc::now().timestamp_nanos_opt().unwrap() & 0x7fff_ffff;
    let create = |agent: Option<Value>| {
        let db = db.clone();
        async move {
            db.campaigns()
                .create(NewCampaign {
                    org_id: OrgId(org),
                    name: "pacing".into(),
                    concurrency_cap: 10,
                    agent,
                })
                .await
                .unwrap()
                .id
                .0
        }
    };
    let agent_name = format!("pace-{org}");
    let c = create(Some(
        json!({"runtime": "livekit", "agent_name": agent_name}),
    ))
    .await;
    let uri = format!("/v1/campaigns/{c}/pacing");

    let (status, v) = send(app.clone(), "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["mode"], "adaptive");
    assert_eq!(v["limit"], 1);
    let pool = &v["pool"];
    assert_eq!(pool["pool"], format!("livekit:{agent_name}"));
    assert_eq!(
        (&pool["target_ms"], &pool["backoff_ms"], &pool["first_k"]),
        (&json!(1000), &json!(1500), &json!(1)),
        "the defaults come from the 2 s rule"
    );
    assert_eq!((&pool["k"], &pool["slow_start"]), (&json!(1), &json!(true)));
    assert_eq!(pool["window"], 0);

    let (status, v) = send(
        app.clone(),
        "PUT",
        &uri,
        Some(json!({
            "mode": "strict",
            "pool": {"target_ms": 800, "backoff_ms": 1200, "first_k": 4, "max_k": 6},
            "reset": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["mode"], "strict");
    assert_eq!(v["pool"]["k"], 4);
    assert_eq!(v["limit"], 1, "strict: the pool proved nothing yet");
    assert_eq!(
        (&v["pool"]["target_ms"], &v["pool"]["max_k"]),
        (&json!(800), &json!(6))
    );

    let (status, v) = send(app.clone(), "PUT", &uri, Some(json!({"mode": "adaptive"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["limit"], 4);
    assert_eq!(v["pool"]["target_ms"], 800, "a mode change keeps the pool");

    for (bad, word) in [
        (json!({"pool": {"target_ms": 1600}}), "backoff_ms"),
        (json!({"pool": {"backoff_ms": 2500}}), "abandonment limit"),
        (json!({"pool": {"first_k": 4, "max_k": 2}}), "max_k"),
        (json!({"pool": {"target": 900}}), "target"),
        (json!({"mode": "fast"}), "fast"),
        (json!({"typo": 1}), "typo"),
    ] {
        let (status, v) = send(app.clone(), "PUT", &uri, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(v["error"].as_str().unwrap().contains(word), "{bad}: {v}");
    }
    let (_, v) = send(app.clone(), "GET", &uri, None).await;
    assert_eq!(
        v["pool"]["target_ms"], 800,
        "a refused change changes nothing"
    );

    let (status, v) = send(app.clone(), "PUT", &uri, Some(json!({"reset": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (&v["pool"]["k"], &v["pool"]["target_ms"]),
        (&json!(4), &json!(800))
    );

    let (status, v) = send(app.clone(), "GET", "/v1/agent-pools", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(v
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["pool"] == format!("livekit:{agent_name}")));

    // Saved settings that do not read: the pool dials one call at a time
    // (the claim fails closed), the GET says so, and a reset repairs it.
    let broken_name = format!("broken-{org}");
    let b = create(Some(
        json!({"runtime": "livekit", "agent_name": broken_name}),
    ))
    .await;
    sqlx::query("INSERT INTO agent_pools (pool, settings) VALUES ($1, '{\"backoff_ms\": 5000}')")
        .bind(format!("livekit:{broken_name}"))
        .execute(db.pool())
        .await
        .unwrap();
    let broken = format!("/v1/campaigns/{b}/pacing");
    let (status, v) = send(app.clone(), "GET", &broken, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("one call at a time"),
        "{v}"
    );
    let (_, v) = send(app.clone(), "GET", "/v1/agent-pools", None).await;
    let entry = v
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pool"] == format!("livekit:{broken_name}"))
        .unwrap()
        .clone();
    assert!(
        entry["error"].as_str().unwrap().contains("backoff_ms"),
        "{entry}"
    );
    let (status, v) = send(app.clone(), "PUT", &broken, Some(json!({"reset": true}))).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        v["pool"]["backoff_ms"], 1500,
        "the reset puts the defaults back"
    );

    let none = create(None).await;
    let (status, _) = send(
        app.clone(),
        "GET",
        &format!("/v1/campaigns/{none}/pacing"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "no agent, no pool");
    let (status, _) = send(app, "GET", "/v1/campaigns/999999999/pacing", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
