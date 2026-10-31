//! HTTP contract tests for the disposition catalog, the DNC list, the
//! attempts, and the campaign events. They need Postgres (DATABASE_URL) and
//! skip without it. Without Postgres, the endpoints answer 503.

use std::sync::Arc;

use ad_api::{router, AppState};
use ad_campaign::repo::{CampaignRepo, NewCampaign};
use ad_core::time::SystemClock;
use ad_core::{CampaignId, OrgId};
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
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn setup() -> Option<(Db, axum::Router, CampaignId)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    let app = router(AppState::postgres(Arc::new(SystemClock), db.clone()));
    let org = Utc::now().timestamp_nanos_opt().unwrap() & 0x7fff_ffff;
    let c = db
        .campaigns()
        .create(NewCampaign {
            org_id: OrgId(org),
            name: "outcomes".into(),
            concurrency_cap: 10,
            agent: None,
        })
        .await
        .unwrap()
        .id;
    Some((db, app, c))
}

#[tokio::test]
async fn without_postgres_the_endpoints_answer_503() {
    let app = router(AppState::new(Arc::new(SystemClock)));
    for uri in [
        "/v1/dnc",
        "/v1/campaigns/1/events",
        "/v1/campaigns/1/dispositions",
    ] {
        let (status, _) = send(app.clone(), "GET", uri, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
    }
}

#[tokio::test]
async fn the_catalog_has_the_system_codes_and_takes_campaign_codes() {
    let Some((_, app, c)) = setup().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let uri = format!("/v1/campaigns/{}/dispositions", c.0);
    let (status, v) = send(app.clone(), "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let system: Vec<_> = v["system"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].clone())
        .collect();
    assert!(system.contains(&json!("SALE")) && system.contains(&json!("DNC")));
    assert_eq!(v["campaign"], json!([]));

    let appt = json!([{"code": "APPT", "name": "Appointment", "flags": {"human_answered": true, "sale": true}}]);
    let (status, v) = send(app.clone(), "PUT", &uri, Some(appt)).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (_, v) = send(app.clone(), "GET", &uri, None).await;
    assert_eq!(v["campaign"][0]["code"], "APPT");
    assert_eq!(v["campaign"][0]["flags"]["sale"], true);

    let clash = json!([{"code": "SALE", "name": "Mine"}]);
    let (status, v) = send(app.clone(), "PUT", &uri, Some(clash)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().contains("system code"));
    let unknown_flag = json!([{"code": "X", "name": "x", "flags": {"vip": true}}]);
    let (status, _) = send(app.clone(), "PUT", &uri, Some(unknown_flag)).await;
    assert!(status.is_client_error());
    let (status, _) = send(app, "GET", "/v1/campaigns/999999999/dispositions", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_dnc_entry_takes_any_form_of_the_number_and_stops_waiting_leads() {
    let Some((_, app, c)) = setup().await else {
        return;
    };
    let number = format!("+1312555{:04}", c.0 % 10_000);
    let (status, _) = send(
        app.clone(),
        "POST",
        &format!("/v1/campaigns/{}/leads", c.0),
        Some(json!({"leads": [{"phone_number": number}]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The national form with a region is the same number.
    let national = format!("({}) {}-{}", &number[2..5], &number[5..8], &number[8..]);
    let add = json!({"phone_number": national, "region": "US", "scope": "campaign",
                     "campaign_id": c.0, "note": "asked by letter"});
    let (status, v) = send(app.clone(), "POST", "/v1/dnc", Some(add.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(
        (v["phone_number"].as_str(), v["cancelled"].as_u64()),
        (Some(number.as_str()), Some(1))
    );
    let (status, v) = send(app.clone(), "POST", "/v1/dnc", Some(add)).await;
    assert_eq!(
        (status, v["created"].as_bool()),
        (StatusCode::OK, Some(false))
    );

    let (_, leads) = send(
        app.clone(),
        "GET",
        &format!("/v1/campaigns/{}/leads", c.0),
        None,
    )
    .await;
    assert_eq!(leads[0]["status"], "dnc");

    let (_, list) = send(
        app.clone(),
        "GET",
        &format!("/v1/dnc?campaign_id={}", c.0),
        None,
    )
    .await;
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["phone_number"] == number.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (entry["scope"].as_str(), entry["source"].as_str()),
        (Some("campaign"), Some("api"))
    );
    assert_eq!(entry["evidence"]["note"], "asked by letter");

    for bad in [
        json!({"phone_number": "312-555-0100"}),
        json!({"phone_number": number, "scope": "campaign"}),
        json!({"phone_number": number, "scope": "system", "campaign_id": c.0}),
        json!({"phone_number": number, "scope": "org"}),
    ] {
        let (status, _) = send(app.clone(), "POST", "/v1/dnc", Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let uri = format!("/v1/dnc/{}", entry["id"]);
    let (status, _) = send(app.clone(), "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(app, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_events_and_the_attempts_of_a_campaign_page_by_id() {
    let Some((db, app, c)) = setup().await else {
        return;
    };
    for t in [
        ad_campaign::Transition::Start,
        ad_campaign::Transition::SyncComplete,
    ] {
        db.campaigns()
            .transition(c, t, Utc::now())
            .await
            .unwrap()
            .unwrap();
    }
    let uri = format!("/v1/campaigns/{}/events", c.0);
    let (status, v) = send(app.clone(), "GET", &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let first = v[0].clone();
    assert_eq!(first["kind"], "campaign_running");
    assert_eq!(first["event"]["campaign_id"], c.0);
    let (_, after) = send(
        app.clone(),
        "GET",
        &format!("{uri}?after={}", first["id"]),
        None,
    )
    .await;
    assert!(after
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["id"].as_i64() > first["id"].as_i64()));

    let (status, v) = send(app, "GET", &format!("/v1/campaigns/{}/attempts", c.0), None).await;
    assert_eq!((status, v), (StatusCode::OK, json!([])));
}
