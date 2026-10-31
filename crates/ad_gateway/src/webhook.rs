//! LiveKit webhooks: the signal that a call ended. LiveKit signs each
//! webhook with the API key: the `Authorization` header is a JWT that holds
//! the SHA-256 of the body. `WebhookVerifier` checks it against the role's
//! clock (OPS-9) and reduces the event to the signals that end a call.

use std::sync::Arc;

use ad_core::Clock;
use ad_livekit::{Signer, TokenError};

/// A webhook that can end a call attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallSignal {
    /// A participant left a room.
    ParticipantLeft { room: String, identity: String },
    /// A room closed. Every participant in it is gone.
    RoomFinished { room: String },
    /// Any other event.
    Other,
}

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("invalid authorization: {0}")]
    Auth(#[from] TokenError),
    #[error("invalid body: {0}")]
    Body(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct WebhookVerifier {
    signer: Signer,
}

impl WebhookVerifier {
    pub fn new(api_key: &str, api_secret: &str, clock: Arc<dyn Clock>) -> Self {
        Self {
            signer: Signer::new(api_key, api_secret, clock),
        }
    }

    /// Verify the signature and read the event.
    pub fn verify(&self, body: &str, authorization: &str) -> Result<CallSignal, WebhookError> {
        self.signer.verify_body(body, authorization)?;
        let event: livekit_protocol::WebhookEvent = serde_json::from_str(body)?;
        let room = event.room.map(|r| r.name).unwrap_or_default();
        Ok(match (event.event.as_str(), event.participant) {
            ("participant_left" | "participant_connection_aborted", Some(p)) => {
                CallSignal::ParticipantLeft {
                    room,
                    identity: p.identity,
                }
            }
            ("room_finished", _) => CallSignal::RoomFinished { room },
            _ => CallSignal::Other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ad_core::time::FixedClock;
    use ad_livekit::Claims;
    use base64::Engine;
    use chrono::{DateTime, Utc};
    use sha2::{Digest, Sha256};

    const KEY: &str = "devkey";
    const SECRET: &str = "devsecret_devsecret_devsecret_32b";

    /// A time far from the OS clock: a check with the OS clock fails.
    fn at() -> DateTime<Utc> {
        DateTime::from_timestamp(1_900_000_000, 0).unwrap()
    }

    fn verifier(at: DateTime<Utc>) -> WebhookVerifier {
        WebhookVerifier::new(KEY, SECRET, Arc::new(FixedClock(at)))
    }

    /// Sign a body the way LiveKit does, at `at()`.
    fn sign(body: &str, secret: &str) -> String {
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        Signer::new(KEY, secret, Arc::new(FixedClock(at())))
            .sign(
                Claims {
                    sha256: hash,
                    ..Default::default()
                },
                std::time::Duration::from_secs(300),
            )
            .unwrap()
    }

    const LEFT: &str = r#"{"event":"participant_left","room":{"name":"campaign-7"},
        "participant":{"identity":"sip-attempt-9"},"id":"EV_1","createdAt":"1800000000"}"#;

    #[test]
    fn a_signed_participant_left_is_read() {
        let v = verifier(at());
        assert_eq!(
            v.verify(LEFT, &sign(LEFT, SECRET)).unwrap(),
            CallSignal::ParticipantLeft {
                room: "campaign-7".into(),
                identity: "sip-attempt-9".into()
            }
        );
    }

    #[test]
    fn a_room_finished_is_read() {
        let body = r#"{"event":"room_finished","room":{"name":"campaign-7"},"id":"EV_2"}"#;
        let v = verifier(at());
        assert_eq!(
            v.verify(body, &sign(body, SECRET)).unwrap(),
            CallSignal::RoomFinished {
                room: "campaign-7".into()
            }
        );
    }

    #[test]
    fn a_wrong_key_or_a_changed_body_is_refused() {
        let v = verifier(at());
        let other_secret = "another_secret_another_secret_32";
        assert!(v.verify(LEFT, &sign(LEFT, other_secret)).is_err());
        let changed = LEFT.replace("sip-attempt-9", "sip-attempt-8");
        assert!(v.verify(&changed, &sign(LEFT, SECRET)).is_err());
        assert!(v.verify(LEFT, "not a token").is_err());
    }

    #[test]
    fn the_token_time_is_checked_against_the_role_clock() {
        let token = sign(LEFT, SECRET);
        assert!(verifier(at() + chrono::Duration::minutes(4))
            .verify(LEFT, &token)
            .is_ok());
        assert!(verifier(at() + chrono::Duration::minutes(7))
            .verify(LEFT, &token)
            .is_err());
        assert!(verifier(at() - chrono::Duration::minutes(2))
            .verify(LEFT, &token)
            .is_err());
    }

    #[test]
    fn other_events_end_nothing() {
        let body = r#"{"event":"track_published","room":{"name":"r"},"id":"EV_3"}"#;
        let v = verifier(at());
        assert_eq!(
            v.verify(body, &sign(body, SECRET)).unwrap(),
            CallSignal::Other
        );
    }
}
