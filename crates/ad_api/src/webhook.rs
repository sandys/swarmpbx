//! LiveKit webhook: the end of a call (GAPS.md DIAL-1, DIAL-2).
//!
//!   POST /v1/webhooks/livekit   LiveKit signs the body with the API key.
//!     200 {"ended": n}   n = the call attempts that this event ended
//!     401                the signature is missing or wrong
//!     503                the API has no LiveKit keys or no Postgres
//!
//! The endpoint applies `hangup` when the callee leaves or the room closes, and
//! `agent_left` when the agent leaves. The attempt statechart ignores a
//! repeated or early event, so LiveKit can deliver a webhook twice.

use axum::{
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::Serialize;

use ad_attempt::AttemptEvent;
use ad_db::{Applied, PgAttemptRepo};
use ad_gateway::{CallSignal, WebhookVerifier};

use crate::AppState;

/// What the webhook endpoint needs. The API has it only when LiveKit keys and
/// Postgres are configured.
#[derive(Clone)]
pub struct Webhooks {
    pub verifier: WebhookVerifier,
    pub attempts: PgAttemptRepo,
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/webhooks/livekit", post(livekit))
}

#[derive(Debug, Serialize)]
pub struct WebhookResult {
    pub ended: u32,
}

#[derive(Debug, Serialize)]
pub struct ErrorDto {
    pub error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorDto>) {
    (status, Json(ErrorDto { error: msg.into() }))
}

async fn livekit(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<WebhookResult>, (StatusCode, Json<ErrorDto>)> {
    let Some(hooks) = &state.webhooks else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "LiveKit webhooks are not configured",
        ));
    };
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "no Authorization header"))?;
    let signal = hooks
        .verifier
        .verify(&body, token)
        .map_err(|e| err(StatusCode::UNAUTHORIZED, e.to_string()))?;
    let (room, left) = match signal {
        CallSignal::ParticipantLeft { room, identity } => (room, Some(identity)),
        CallSignal::RoomFinished { room } => (room, None),
        CallSignal::Other => return Ok(Json(WebhookResult { ended: 0 })),
    };

    let storage = |e: ad_db::AttemptError| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    let at = state.clock.now_utc();
    let mut ended = 0;
    for a in hooks.attempts.live_in_room(&room).await.map_err(storage)? {
        // Who left decides the event: the callee hangs up, or the agent ends
        // the call. Anyone else does not change the attempt.
        let event = match &left {
            None => AttemptEvent::Hangup {
                at,
                detail: "the room closed".into(),
            },
            Some(identity) if *identity == a.callee_identity => AttemptEvent::Hangup {
                at,
                detail: "the call left the room".into(),
            },
            Some(identity) if a.agent_identity.as_deref() == Some(identity.as_str()) => {
                AttemptEvent::AgentLeft { at }
            }
            Some(_) => continue,
        };
        if let Applied::Changed(next) = hooks
            .attempts
            .apply(a.attempt_id, &event)
            .await
            .map_err(storage)?
        {
            if !next.leg.is_live() {
                ended += 1;
            }
        }
    }
    if ended > 0 {
        tracing::info!(room, ended, "webhook ended call attempts");
    }
    Ok(Json(WebhookResult { ended }))
}
