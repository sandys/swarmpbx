//! HTTP contract tests for the hopper preview endpoint. This is the end-to-end
//! proof that routing + compliance + leads work together: load leads, preview,
//! and assert which are eligible and why the rest are held.

use std::sync::Arc;

use ad_api::{router, AppState};
use ad_core::time::FixedClock;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// Clock at 15:00 UTC = 11:00 America/New_York (inside a 9am-9pm window).
fn app() -> axum::Router {
    let clock = FixedClock(ad_compliance::safe_hours::utc(2026, 6, 1, 15, 0));
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

async fn load_leads(app: &axum::Router, cid: i64, leads: Value) {
    send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{cid}/leads"),
        Some(json!({ "leads": leads })),
    )
    .await;
}

#[tokio::test]
async fn preview_selects_eligible_and_excludes_by_compliance() {
    let app = app();
    let cid = make_campaign(&app).await;
    load_leads(
        &app,
        cid,
        json!([
            { "phone_number": "+13125550100", "timezone": "America/New_York" }, // eligible
            { "phone_number": "+13125550101" }                                   // no tz -> fail closed
        ]),
    )
    .await;

    let (status, v) = send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{cid}/hopper/preview"),
        Some(json!({ "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 } })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["eligible"].as_array().unwrap().len(), 1);
    assert_eq!(v["eligible"][0]["phone_number"], "+13125550100");
    assert_eq!(v["excluded"].as_array().unwrap().len(), 1);
    assert!(v["excluded"][0]["reason"]
        .as_str()
        .unwrap()
        .starts_with("compliance:"));
}

#[tokio::test]
async fn preview_excludes_by_routing_rule() {
    let app = app();
    let cid = make_campaign(&app).await;
    load_leads(
        &app,
        cid,
        json!([
            { "phone_number": "+1ny", "timezone": "America/New_York", "state": "NY" },
            { "phone_number": "+1tx", "timezone": "America/New_York", "state": "TX" }
        ]),
    )
    .await;

    // Ruleset: skip any lead in state TX.
    let (status, v) = send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{cid}/hopper/preview"),
        Some(json!({
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 },
            "ruleset": [
                { "tag": "condition", "field_or_verb": "state", "data": "^TX$", "group": 0, "order": 0 },
                { "tag": "action", "field_or_verb": "skip", "data": "", "group": 0, "order": 10 }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["eligible"].as_array().unwrap().len(), 1);
    assert_eq!(v["eligible"][0]["phone_number"], "+1ny");
    assert_eq!(v["excluded"].as_array().unwrap().len(), 1);
    assert_eq!(v["excluded"][0]["reason"], "routing:skip");
}

#[tokio::test]
async fn preview_on_missing_campaign_is_404() {
    let (status, _) = send(
        app(),
        "POST",
        "/v1/campaigns/999/hopper/preview",
        Some(json!({ "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 } })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn preview_bad_window_is_400() {
    let app = app();
    let cid = make_campaign(&app).await;
    let (status, _) = send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{cid}/hopper/preview"),
        Some(json!({ "uniform_window": { "start_hhmm": 2599, "stop_hhmm": 2100 } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn preview_up_count_order() {
    let app = app();
    let cid = make_campaign(&app).await;
    load_leads(
        &app,
        cid,
        json!([
            { "phone_number": "+1a", "timezone": "America/New_York" },
            { "phone_number": "+1b", "timezone": "America/New_York" }
        ]),
    )
    .await;
    // Both fresh (called_count 0) -> order falls back to id ascending.
    let (status, v) = send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{cid}/hopper/preview"),
        Some(json!({ "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 }, "order": "up_count" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let e = v["eligible"].as_array().unwrap();
    assert_eq!(e.len(), 2);
    assert_eq!(e[0]["phone_number"], "+1a");
}
