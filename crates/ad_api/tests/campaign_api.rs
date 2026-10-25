//! HTTP contract tests for the campaign API. In-process via tower oneshot.
//! One router/state per test, so the in-memory repo starts empty each time.

use std::sync::Arc;

use ad_api::{router, AppState};
use ad_core::time::FixedClock;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> axum::Router {
    let clock = FixedClock(ad_compliance::safe_hours::utc(2026, 6, 1, 12, 0));
    router(AppState::new(Arc::new(clock)))
}

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
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn create_returns_201_and_draft_state() {
    let (status, v) = send(
        app(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 7, "name": "spring sale" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["org_id"], 7);
    assert_eq!(v["name"], "spring sale");
    assert_eq!(v["state"], "draft");
    assert_eq!(v["id"], 1);
    assert_eq!(v["concurrency_cap"], 10, "the default cap");
}

#[tokio::test]
async fn create_rejects_empty_name_400() {
    let (status, _) = send(
        app(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 1, "name": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_missing_is_404() {
    let (status, _) = send(app(), "GET", "/v1/campaigns/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_then_get_and_list() {
    let app = app();
    let (c, created) = send(
        app.clone(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 3, "name": "a" })),
    )
    .await;
    assert_eq!(c, StatusCode::CREATED);
    let id = created["id"].as_i64().unwrap();

    let (g, got) = send(app.clone(), "GET", &format!("/v1/campaigns/{id}"), None).await;
    assert_eq!(g, StatusCode::OK);
    assert_eq!(got["id"], id);

    let (l, listed) = send(app.clone(), "GET", "/v1/campaigns?org_id=3", None).await;
    assert_eq!(l, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // Another org sees an empty list.
    let (_, other) = send(app, "GET", "/v1/campaigns?org_id=4", None).await;
    assert_eq!(other.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn transition_start_moves_draft_to_syncing() {
    let app = app();
    let (_, created) = send(
        app.clone(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 1, "name": "x" })),
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    let (s, v) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{id}/transition"),
        Some(json!({ "transition": "start" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["state"], "syncing");
}

#[tokio::test]
async fn illegal_transition_is_409() {
    let app = app();
    let (_, created) = send(
        app.clone(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 1, "name": "x" })),
    )
    .await;
    let id = created["id"].as_i64().unwrap();

    // Pause from Draft is illegal.
    let (s, v) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{id}/transition"),
        Some(json!({ "transition": "pause" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(v["error"].as_str().unwrap().contains("illegal transition"));
}

#[tokio::test]
async fn transition_on_missing_campaign_is_404() {
    let (s, _) = send(
        app(),
        "POST",
        "/v1/campaigns/999/transition",
        Some(json!({ "transition": "start" })),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_takes_a_concurrency_cap_and_rejects_zero() {
    let (status, v) = send(
        app(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 7, "name": "big", "concurrency_cap": 250 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["concurrency_cap"], 250);

    let (status, v) = send(
        app(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 7, "name": "none", "concurrency_cap": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("concurrency_cap"));
}

#[tokio::test]
async fn create_takes_an_agent_and_rejects_a_bad_one() {
    let agent = json!({"runtime": "pipecat", "start_url": "http://bots:7860/start"});
    let (status, v) = send(
        app(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 7, "name": "bots", "agent": agent })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["agent"]["runtime"], "pipecat");

    for bad in [
        json!({"runtime": "livekit"}),
        json!({"runtime": "unknown", "x": 1}),
        json!({"runtime": "pipecat", "start_url": "not a url"}),
    ] {
        let (status, v) = send(
            app(),
            "POST",
            "/v1/campaigns",
            Some(json!({ "org_id": 7, "name": "bad", "agent": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(
            v["error"].as_str().unwrap().contains("invalid agent"),
            "{v}"
        );
    }
}

#[tokio::test]
async fn the_dial_order_is_part_of_the_agent() {
    let agent = |order: &str| {
        json!({ "org_id": 7, "name": "x",
                "agent": {"runtime": "livekit", "agent_name": "sales", "dial_order": order} })
    };
    let (status, v) = send(app(), "POST", "/v1/campaigns", Some(agent("agent_first"))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["agent"]["dial_order"], "agent_first");
    let (status, v) = send(app(), "POST", "/v1/campaigns", Some(agent("agent_last"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("agent_last"), "{v}");
}

#[tokio::test]
async fn the_abandonment_ledger_needs_postgres() {
    let (status, v) = send(app(), "GET", "/v1/campaigns/1/abandonment", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(v["error"].as_str().unwrap().contains("Postgres"));
}
