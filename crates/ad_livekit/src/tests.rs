use std::sync::Arc;
use std::time::Duration;

use ad_core::time::{FixedClock, SystemClock};
use ad_core::Clock;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Utc};
use livekit_api::access_token::{AccessToken, TokenVerifier};
use livekit_protocol as proto;
use prost::Message;
use sha2::{Digest, Sha256};

use super::*;

const KEY: &str = "devkey";
const SECRET: &str = "devsecret_devsecret_devsecret_32b";

fn signer(clock: impl Clock + 'static) -> Signer {
    Signer::new(KEY, SECRET, Arc::new(clock))
}

fn hour_ahead() -> DateTime<Utc> {
    Utc::now() + chrono::Duration::hours(1)
}

fn join_grants() -> VideoGrants {
    VideoGrants {
        can_publish: Some(true),
        can_subscribe: Some(true),
        ..Default::default()
    }
}

#[test]
fn livekit_accepts_a_token_of_the_signer() {
    let token = signer(SystemClock)
        .join(
            "room-1",
            "sip-attempt-1",
            Some("agent"),
            join_grants(),
            Duration::from_secs(60),
        )
        .unwrap();
    let claims = TokenVerifier::with_api_key(KEY, SECRET)
        .verify(&token)
        .unwrap();
    assert_eq!(claims.iss, KEY);
    assert_eq!(claims.sub, "sip-attempt-1");
    assert_eq!(claims.kind, "agent");
    assert!(claims.video.room_join);
    assert_eq!(claims.video.room, "room-1");
    assert_eq!(claims.exp - claims.nbf, 60);
}

#[test]
fn the_token_time_comes_from_the_clock_not_the_os_clock() {
    let at = hour_ahead();
    let token = signer(FixedClock(at))
        .sign(Claims::default(), Duration::from_secs(600))
        .unwrap();
    let claims = Claims::from_unverified(&token).unwrap();
    assert_eq!(claims.nbf as i64, at.timestamp());
    assert_eq!(claims.exp as i64, at.timestamp() + 600);
    // LiveKit checks `nbf` with its own clock: a token from the future fails.
    assert!(TokenVerifier::with_api_key(KEY, SECRET)
        .verify(&token)
        .is_err());
}

#[test]
fn a_join_token_needs_an_identity_and_keys() {
    let s = signer(SystemClock);
    assert_eq!(
        s.join("room-1", "", None, join_grants(), Duration::from_secs(60)),
        Err(TokenError::JoinWithoutIdentity)
    );
    let no_keys = Signer::new(KEY, "", Arc::new(SystemClock));
    assert_eq!(
        no_keys.sign(Claims::default(), Duration::from_secs(60)),
        Err(TokenError::NoKeys)
    );
}

#[test]
fn verify_checks_the_times_against_the_clock() {
    let at = hour_ahead();
    // A LiveKit token from the OS clock is an hour old for a clock an hour
    // ahead, and expired after its 10 minutes.
    let os_token = AccessToken::with_api_key(KEY, SECRET)
        .with_ttl(Duration::from_secs(600))
        .to_jwt()
        .unwrap();
    assert_eq!(
        signer(FixedClock(at)).verify(&os_token),
        Err(TokenError::Expired)
    );
    assert!(signer(SystemClock).verify(&os_token).is_ok());

    let ahead = signer(FixedClock(at))
        .sign(Claims::default(), Duration::from_secs(600))
        .unwrap();
    assert!(signer(FixedClock(at)).verify(&ahead).is_ok());
    assert_eq!(
        signer(SystemClock).verify(&ahead),
        Err(TokenError::NotYetValid)
    );
    // Within the leeway.
    let in_leeway = FixedClock(at - chrono::Duration::seconds(50));
    assert!(signer(in_leeway).verify(&ahead).is_ok());
}

#[test]
fn verify_refuses_a_wrong_signature_issuer_or_form() {
    let token = signer(SystemClock)
        .sign(Claims::default(), Duration::from_secs(60))
        .unwrap();
    let other = Signer::new(
        KEY,
        "another_secret_another_secret_32",
        Arc::new(SystemClock),
    );
    assert_eq!(other.verify(&token), Err(TokenError::Signature));
    let other_key = Signer::new("otherkey", SECRET, Arc::new(SystemClock));
    let foreign = other_key
        .sign(Claims::default(), Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        signer(SystemClock).verify(&foreign),
        Err(TokenError::Issuer)
    );
    assert_eq!(
        signer(SystemClock).verify("not a token"),
        Err(TokenError::Malformed)
    );
    assert!(signer(SystemClock)
        .verify(&format!("Bearer {token}"))
        .is_ok());
}

#[test]
fn verify_body_checks_the_hash_of_the_body() {
    let body = r#"{"event":"room_finished","room":{"name":"r"}}"#;
    let s = signer(SystemClock);
    let token = s
        .sign(
            Claims {
                sha256: STANDARD.encode(Sha256::digest(body)),
                ..Default::default()
            },
            Duration::from_secs(300),
        )
        .unwrap();
    assert!(s.verify_body(body, &token).is_ok());
    assert_eq!(
        s.verify_body(&body.replace("\"r\"", "\"q\""), &token),
        Err(TokenError::Body)
    );
}

/// What the mock server saw: the path, the token, and the body.
type Seen = Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>;

async fn mock(answer: StatusCode, reply: Vec<u8>) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let s = seen.clone();
    let app = Router::new().route(
        "/twirp/*rest",
        post(
            move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
                let s = s.clone();
                let reply = reply.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    s.lock()
                        .unwrap()
                        .push((uri.path().to_owned(), auth, body.to_vec()));
                    (answer, reply)
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("ws://{addr}"), seen)
}

#[tokio::test]
async fn a_request_carries_a_token_from_the_clock_with_its_grants() {
    let reply = proto::ListParticipantsResponse {
        participants: vec![proto::ParticipantInfo {
            identity: "agent-1".into(),
            ..Default::default()
        }],
    }
    .encode_to_vec();
    let (url, seen) = mock(StatusCode::OK, reply).await;
    let at = hour_ahead();
    let api = Api::new(&url, signer(FixedClock(at)));
    let list = api.list_participants("room-7").await.unwrap();
    assert_eq!(list[0].identity, "agent-1");

    let (path, auth, body) = seen.lock().unwrap()[0].clone();
    assert_eq!(path, "/twirp/livekit.RoomService/ListParticipants");
    assert_eq!(
        proto::ListParticipantsRequest::decode(&body[..])
            .unwrap()
            .room,
        "room-7"
    );
    let claims = signer(FixedClock(at)).verify(&auth).unwrap();
    assert_eq!(claims.nbf as i64, at.timestamp());
    assert!(claims.video.room_admin);
    assert_eq!(claims.video.room, "room-7");
}

#[tokio::test]
async fn a_twirp_error_keeps_its_code() {
    let (url, _) = mock(
        StatusCode::NOT_FOUND,
        br#"{"code":"not_found","msg":"room not found"}"#.to_vec(),
    )
    .await;
    let api = Api::new(&url, signer(SystemClock));
    let e = api.delete_room("gone").await.unwrap_err();
    assert!(e.is_not_found(), "{e}");
    let e = api.get_participant("gone", "x").await.unwrap_err();
    assert!(matches!(e, ApiError::Server { status: 404, .. }), "{e}");

    let (url, _) = mock(StatusCode::UNAUTHORIZED, b"no".to_vec()).await;
    let e = Api::new(&url, signer(SystemClock))
        .create_room(proto::CreateRoomRequest::default())
        .await
        .unwrap_err();
    assert!(!e.is_not_found());
    assert!(matches!(e, ApiError::Server { status: 401, ref code, .. } if code == "unknown"));
}

#[tokio::test]
async fn a_dispatch_is_signed_for_its_room() {
    let reply = proto::AgentDispatch {
        id: "AD_1".into(),
        ..Default::default()
    }
    .encode_to_vec();
    let (url, seen) = mock(StatusCode::OK, reply).await;
    let api = Api::new(&url, signer(SystemClock));
    let d = api
        .create_dispatch(proto::CreateAgentDispatchRequest {
            agent_name: "sales".into(),
            room: "room-9".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(d.id, "AD_1");
    let (path, auth, _) = seen.lock().unwrap()[0].clone();
    assert_eq!(path, "/twirp/livekit.AgentDispatchService/CreateDispatch");
    let claims = signer(SystemClock).verify(&auth).unwrap();
    assert_eq!(claims.video.room, "room-9");
    assert!(claims.video.room_admin);
}
