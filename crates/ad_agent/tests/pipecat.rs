//! The pipecat adapter against a fake bot server: the start request carries
//! the room, a token for that room only, and the call facts.

use std::sync::{Arc, Mutex};

use ad_agent::{AgentError, AgentSpec, Agents, CallFacts, LiveKitAccess};
use ad_core::time::{FixedClock, SystemClock};
use ad_core::Clock;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use livekit_api::access_token::{Claims, TokenVerifier};
use serde_json::{json, Value};

const KEY: &str = "devkey";
const SECRET: &str = "devsecret_devsecret_devsecret_32b";

type Seen = Arc<Mutex<Vec<Value>>>;

/// A bot server that records each start request and answers `status`.
async fn bot_server(status: StatusCode) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route(
            "/start",
            post(|State((seen, status)): State<(Seen, StatusCode)>, Json(body): Json<Value>| async move {
                seen.lock().unwrap().push(body);
                status
            }),
        )
        .with_state((seen.clone(), status));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/start"), seen)
}

fn agents() -> Agents {
    agents_at(SystemClock)
}

fn agents_at(clock: impl Clock + 'static) -> Agents {
    Agents::new(
        LiveKitAccess {
            url: "ws://livekit.test:7880".into(),
            api_key: KEY.into(),
            api_secret: SECRET.into(),
        },
        Arc::new(clock),
    )
}

fn call(context: &Value) -> CallFacts<'_> {
    CallFacts {
        attempt_id: 42,
        campaign_id: 7,
        lead_id: 9,
        room: "campaign-42",
        callee_identity: "sip-attempt-42",
        phone_number: "+15550100",
        lead_context: context,
        session: None,
    }
}

fn spec(start_url: &str) -> AgentSpec {
    AgentSpec::parse(
        &json!({"runtime": "pipecat", "start_url": start_url, "metadata": {"voice": "x"}}),
    )
    .unwrap()
}

#[tokio::test]
async fn the_bot_gets_a_room_token_and_the_call_facts() {
    let (url, seen) = bot_server(StatusCode::OK).await;
    let context = json!({"first_name": "Ada"});
    let detail = agents().start(&spec(&url), &call(&context)).await.unwrap();
    assert_eq!(detail, "pipecat bot agent-attempt-42");

    let body = seen.lock().unwrap().pop().expect("one start request");
    assert_eq!(body["url"], "ws://livekit.test:7880");
    assert_eq!(body["room_name"], "campaign-42");
    assert_eq!(body["identity"], "agent-attempt-42");
    assert_eq!(body["call"]["callee_identity"], "sip-attempt-42");
    assert_eq!(body["call"]["lead"]["context"]["first_name"], "Ada");
    assert_eq!(body["call"]["metadata"]["voice"], "x");

    let claims = TokenVerifier::with_api_key(KEY, SECRET)
        .verify(body["token"].as_str().unwrap())
        .expect("the token is signed with the LiveKit key");
    assert_eq!(claims.sub, "agent-attempt-42");
    assert_eq!(claims.video.room, "campaign-42");
    assert!(claims.video.room_join && claims.video.agent);
    assert!(!claims.video.room_admin && !claims.video.room_create);
}

#[tokio::test]
async fn the_bot_token_takes_its_time_from_the_clock() {
    let (url, seen) = bot_server(StatusCode::OK).await;
    let at = chrono::DateTime::from_timestamp(1_900_000_000, 0).unwrap();
    let context = json!({});
    agents_at(FixedClock(at))
        .start(&spec(&url), &call(&context))
        .await
        .unwrap();
    let body = seen.lock().unwrap().pop().expect("one start request");
    let claims = Claims::from_unverified(body["token"].as_str().unwrap()).unwrap();
    assert_eq!(claims.nbf as i64, at.timestamp());
    assert_eq!(claims.exp as i64, at.timestamp() + 3600);
}

#[tokio::test]
async fn a_bot_server_that_refuses_fails_the_start() {
    let (url, _) = bot_server(StatusCode::SERVICE_UNAVAILABLE).await;
    let context = json!({});
    let err = agents()
        .start(&spec(&url), &call(&context))
        .await
        .unwrap_err();
    assert!(
        matches!(err, AgentError::Refused(ref m) if m.contains("503")),
        "{err}"
    );
}

#[tokio::test]
async fn an_unreachable_bot_server_fails_the_start() {
    let context = json!({});
    let err = agents()
        .start(&spec("http://127.0.0.1:9/start"), &call(&context))
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::Unreachable(_)), "{err}");
}
