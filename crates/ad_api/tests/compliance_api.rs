//! HTTP contract tests for the compliance API. In-process via tower oneshot —
//! no network, deterministic clock. Each test exercises the real router + real
//! ad_compliance engine, asserting the JSON contract.

use std::sync::Arc;

use ad_api::{router, AppState};
use ad_core::time::FixedClock;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

/// Router with the clock frozen at a known instant.
fn app_at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> axum::Router {
    let clock = FixedClock(ad_compliance::safe_hours::utc(y, mo, d, h, mi));
    router(AppState::new(Arc::new(clock)))
}

async fn post_json(app: axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

#[tokio::test]
async fn health_ok() {
    let app = app_at(2026, 6, 1, 12, 0);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["status"], "ok");
}

#[tokio::test]
async fn check_allowed_within_window() {
    // 15:00 UTC = 11:00 EDT on a Monday -> inside 9am-9pm.
    let app = app_at(2026, 6, 1, 15, 0);
    let (status, v) = post_json(
        app,
        "/v1/compliance/check",
        json!({
            "timezone": "America/New_York",
            "phone_number": "+1-312-555-0100",
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["allowed"], true);
    assert_eq!(v["reason"], Value::Null);
}

#[tokio::test]
async fn check_denied_outside_window() {
    // 02:00 UTC Mon = 22:00 EDT Sun -> outside 9am-9pm.
    let app = app_at(2026, 6, 1, 2, 0);
    let (status, v) = post_json(
        app,
        "/v1/compliance/check",
        json!({
            "timezone": "America/New_York",
            "phone_number": "+1-312-555-0100",
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["allowed"], false);
    assert_eq!(v["reason"], "outside_calling_window");
}

#[tokio::test]
async fn check_fail_closed_unknown_timezone() {
    let app = app_at(2026, 6, 1, 15, 0);
    let (status, v) = post_json(
        app,
        "/v1/compliance/check",
        json!({
            "phone_number": "+1-312-555-0100",
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["allowed"], false);
    assert_eq!(v["reason"], "unknown_local_time");
}

#[tokio::test]
async fn check_denied_dnc_hit() {
    // Inside the window, but the number is on the system DNC list.
    let app = app_at(2026, 6, 1, 15, 0);
    let (status, v) = post_json(
        app,
        "/v1/compliance/check",
        json!({
            "timezone": "America/New_York",
            "phone_number": "+1-800-555-1234",
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 },
            "dnc": { "system": ["+18005551234"] }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["allowed"], false);
    assert_eq!(v["reason"], "dnc");
}

#[tokio::test]
async fn check_dnc_matches_another_form_and_refuses_an_entry_that_does_not_read() {
    let app = app_at(2026, 6, 1, 15, 0);
    let req = |entry: &str| {
        json!({
            "timezone": "America/New_York",
            "phone_number": "+13125550100",
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 },
            "dnc": { "campaign": [entry] }
        })
    };
    let (status, v) = post_json(
        app.clone(),
        "/v1/compliance/check",
        req("+1 (312) 555-0100"),
    )
    .await;
    assert_eq!(
        (status, v["reason"].as_str()),
        (StatusCode::OK, Some("dnc"))
    );
    let (status, v) = post_json(app.clone(), "/v1/compliance/check", req("312-555-0100")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["allowed"], false);
    assert!(
        v["reason"].as_str().unwrap().starts_with("invalid_dnc"),
        "{v}"
    );
    let (status, v) = post_json(
        app,
        "/v1/compliance/check",
        json!({
            "timezone": "America/New_York",
            "phone_number": "312-555-0100",
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 },
            "dnc": {}
        }),
    )
    .await;
    assert_eq!(
        (status, v["reason"].as_str()),
        (StatusCode::OK, Some("invalid_number"))
    );
}

#[tokio::test]
async fn check_bad_window_is_400_and_not_allowed() {
    let app = app_at(2026, 6, 1, 15, 0);
    let (status, v) = post_json(
        app,
        "/v1/compliance/check",
        json!({
            "timezone": "America/New_York",
            "phone_number": "+1-312-555-0100",
            "uniform_window": { "start_hhmm": 2599, "stop_hhmm": 2100 }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(v["allowed"], false);
}

#[tokio::test]
async fn check_holiday_denied() {
    // Inside window but 2026-07-04 is a holiday.
    let app = app_at(2026, 7, 4, 16, 0);
    let (status, v) = post_json(
        app,
        "/v1/compliance/check",
        json!({
            "timezone": "America/New_York",
            "phone_number": "+1-312-555-0100",
            "uniform_window": { "start_hhmm": 900, "stop_hhmm": 2100 },
            "holidays": ["2026-07-04"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["allowed"], false);
    assert_eq!(v["reason"], "holiday");
}
