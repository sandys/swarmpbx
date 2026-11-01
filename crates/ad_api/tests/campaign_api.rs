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

#[tokio::test]
async fn patch_changes_only_the_given_settings_in_any_state() {
    let app = app();
    let agent = json!({"runtime": "livekit", "agent_name": "a"});
    let (_, c) = send(
        app.clone(),
        "POST",
        "/v1/campaigns",
        Some(json!({"org_id": 1, "name": "spring", "concurrency_cap": 2, "agent": agent})),
    )
    .await;
    let id = c["id"].as_i64().unwrap();
    let uri = format!("/v1/campaigns/{id}");
    for t in ["start", "sync_complete"] {
        send(
            app.clone(),
            "POST",
            &format!("{uri}/transition"),
            Some(json!({"transition": t})),
        )
        .await;
    }

    let (status, v) = send(
        app.clone(),
        "PATCH",
        &uri,
        Some(json!({"concurrency_cap": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        (&v["name"], &v["concurrency_cap"], &v["agent"], &v["state"]),
        (&json!("spring"), &json!(5), &agent, &json!("running"))
    );

    let other =
        json!({"runtime": "pipecat", "start_url": "http://bot/start", "dial_order": "agent_first"});
    let (status, v) = send(
        app.clone(),
        "PATCH",
        &uri,
        Some(json!({"name": "autumn", "agent": other})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        (&v["name"], &v["concurrency_cap"], &v["agent"]),
        (&json!("autumn"), &json!(5), &other)
    );
    let (_, got) = send(app, "GET", &uri, None).await;
    assert_eq!(got, v);
}

#[tokio::test]
async fn retry_settings_have_defaults_and_change_by_key() {
    let app = app();
    let (status, c) = send(
        app.clone(),
        "POST",
        "/v1/campaigns",
        Some(json!({"org_id": 1, "name": "fast", "retry": {"busy_s": 60}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    assert_eq!(
        c["retry"],
        json!({"busy_s": 60, "no_answer_s": 1800, "voicemail_s": 3600, "failed_s": 900,
               "error_s": 900, "abandoned_s": 900, "max_calls": 3, "callback_min_s": 60})
    );
    let uri = format!("/v1/campaigns/{}", c["id"]);
    let (status, v) = send(
        app.clone(),
        "PATCH",
        &uri,
        Some(json!({"retry": {"max_calls": 5, "callback_min_s": 5}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(
        (
            &v["retry"]["busy_s"],
            &v["retry"]["max_calls"],
            &v["retry"]["callback_min_s"]
        ),
        (&json!(60), &json!(5), &json!(5)),
        "the other keys keep their values"
    );
    for bad in [
        json!({"retry": {"busy_s": 0}}),
        json!({"retry": {"max_calls": 101}}),
        json!({"retry": {"bogus_s": 5}}),
        json!({"retry": {"busy_s": "5"}}),
        json!({"retry": 5}),
    ] {
        let (status, e) = send(app.clone(), "PATCH", &uri, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {e}");
        let mut create = json!({"org_id": 1, "name": "bad"});
        create["retry"] = bad["retry"].clone();
        let (status, _) = send(app.clone(), "POST", "/v1/campaigns", Some(create)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "create with {bad}");
    }
    let (_, got) = send(app, "GET", &uri, None).await;
    assert_eq!(got["retry"], v["retry"], "a refused change changed nothing");
}

#[tokio::test]
async fn patch_refuses_bad_settings_and_a_missing_campaign() {
    let app = app();
    let (_, c) = send(
        app.clone(),
        "POST",
        "/v1/campaigns",
        Some(json!({"org_id": 1, "name": "spring"})),
    )
    .await;
    let uri = format!("/v1/campaigns/{}", c["id"]);
    for bad in [
        json!({"concurrency_cap": 0}),
        json!({"name": " "}),
        json!({"agent": {"runtime": "livekit", "agent_name": ""}}),
        json!({"agent": {"runtime": "pipecat", "start_url": "bot"}}),
        json!({"cap": 3}),
    ] {
        let (status, _) = send(app.clone(), "PATCH", &uri, Some(bad.clone())).await;
        assert!(status.is_client_error(), "{bad}: {status}");
        let (_, got) = send(app.clone(), "GET", &uri, None).await;
        assert_eq!(got, c, "{bad} changed the campaign");
    }
    let (status, _) = send(
        app,
        "PATCH",
        "/v1/campaigns/999",
        Some(json!({"concurrency_cap": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
