//! HTTP contract tests for the lead API (in-process, in-memory store).

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
    let mut b = Request::builder().method(method).uri(uri);
    let req = if let Some(j) = body {
        b = b.header("content-type", "application/json");
        b.body(Body::from(j.to_string())).unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    };
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Create a campaign and return its id.
async fn make_campaign(app: &axum::Router) -> i64 {
    let (_, v) = send(
        app.clone(),
        "POST",
        "/v1/campaigns",
        Some(json!({ "org_id": 1, "name": "c" })),
    )
    .await;
    v["id"].as_i64().unwrap()
}

#[tokio::test]
async fn bulk_load_returns_201_and_leads() {
    let app = app();
    let cid = make_campaign(&app).await;
    let (status, v) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{cid}/leads"),
        Some(json!({
            "leads": [
                { "phone_number": "+13125550100", "timezone": "America/New_York",
                  "context": { "customer_name": "Jane" } },
                { "phone_number": "+13125550101" }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(v["created"], 2);
    assert_eq!(v["leads"][0]["status"], "new");
    assert_eq!(v["leads"][0]["context"]["customer_name"], "Jane");
}

#[tokio::test]
async fn bulk_load_on_missing_campaign_is_404() {
    let (status, _) = send(
        app(),
        "POST",
        "/v1/campaigns/999/leads",
        Some(json!({ "leads": [{ "phone_number": "+1" }] })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn empty_phone_is_400() {
    let app = app();
    let cid = make_campaign(&app).await;
    let (status, _) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{cid}/leads"),
        Some(json!({ "leads": [{ "phone_number": "   " }] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_leads_is_400() {
    let app = app();
    let cid = make_campaign(&app).await;
    let (status, _) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{cid}/leads"),
        Some(json!({ "leads": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn load_then_list_scoped_to_campaign() {
    let app = app();
    let c1 = make_campaign(&app).await;
    let c2 = make_campaign(&app).await;

    send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{c1}/leads"),
        Some(json!({ "leads": [{ "phone_number": "+1a" }, { "phone_number": "+1b" }] })),
    )
    .await;
    send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{c2}/leads"),
        Some(json!({ "leads": [{ "phone_number": "+2a" }] })),
    )
    .await;

    let (s1, l1) = send(
        app.clone(),
        "GET",
        &format!("/v1/campaigns/{c1}/leads"),
        None,
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(l1.as_array().unwrap().len(), 2);

    let (_, l2) = send(app, "GET", &format!("/v1/campaigns/{c2}/leads"), None).await;
    assert_eq!(l2.as_array().unwrap().len(), 1);
}
