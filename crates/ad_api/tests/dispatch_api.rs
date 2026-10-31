//! HTTP contract tests for the dispatch run-batch endpoint — the dial loop
//! end to end over the stub initiator.

use std::sync::Arc;

use ad_api::{router, AppState};
use ad_core::time::FixedClock;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> axum::Router {
    let clock = FixedClock(ad_compliance::safe_hours::utc(2026, 6, 1, 15, 0)); // 11:00 ET
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

async fn transition(app: &axum::Router, cid: i64, t: &str) {
    send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{cid}/transition"),
        Some(json!({ "transition": t })),
    )
    .await;
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

fn window() -> Value {
    json!({ "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 } })
}

#[tokio::test]
async fn draft_campaign_is_held_not_running() {
    let app = app();
    let cid = make_campaign(&app).await; // starts Draft
    let (status, v) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{cid}/dispatch/run-batch"),
        Some(window()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(v["error"], "hold:not_running");
}

#[tokio::test]
async fn running_campaign_dispatches_eligible_and_skips_noncompliant() {
    let app = app();
    let cid = make_campaign(&app).await;
    load_leads(
        &app,
        cid,
        json!([
            { "phone_number": "+13125550100", "timezone": "America/New_York" }, // eligible
            { "phone_number": "+13125550101" }                                   // no tz -> denied
        ]),
    )
    .await;
    transition(&app, cid, "start").await;
    transition(&app, cid, "sync_complete").await; // now Running

    let (status, v) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{cid}/dispatch/run-batch"),
        Some(window()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The stub initiator is surfaced as experimental, never presented as real.
    assert_eq!(v["experimental"], true);
    assert_eq!(v["initiator"], "stub");
    assert_eq!(v["gate"], "dispatch");
    // Only the eligible lead is dispatched (the no-tz lead was held by the
    // hopper's compliance gate before dispatch).
    assert_eq!(v["dispatched"], 1);
    let outcomes = v["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["outcome"], "answered");
}

#[tokio::test]
async fn capacity_cap_limits_attempts() {
    let app = app();
    let cid = make_campaign(&app).await;
    load_leads(
        &app,
        cid,
        json!([
            { "phone_number": "+13125550201", "timezone": "America/New_York" },
            { "phone_number": "+13125550202", "timezone": "America/New_York" },
            { "phone_number": "+13125550203", "timezone": "America/New_York" }
        ]),
    )
    .await;
    transition(&app, cid, "start").await;
    transition(&app, cid, "sync_complete").await;

    // max_concurrent 0 -> no slot ever available -> all report no_capacity,
    // nothing dispatched.
    let (status, v) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{cid}/dispatch/run-batch"),
        Some(json!({ "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 }, "max_concurrent": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["dispatched"], 0);
    let outcomes = v["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes.iter().all(|o| o["outcome"] == "no_capacity"));
}

#[tokio::test]
async fn run_batch_on_missing_campaign_is_404() {
    let (status, _) = send(
        app(),
        "POST",
        "/v1/campaigns/999/dispatch/run-batch",
        Some(window()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_window_is_400() {
    let app = app();
    let cid = make_campaign(&app).await;
    transition(&app, cid, "start").await;
    transition(&app, cid, "sync_complete").await;
    let (status, _) = send(
        app,
        "POST",
        &format!("/v1/campaigns/{cid}/dispatch/run-batch"),
        Some(json!({ "uniform_window": { "start_hhmm": 2599, "stop_hhmm": 2100 } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
