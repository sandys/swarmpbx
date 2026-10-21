//! LiveKit webhooks: the signal that a call ended. LiveKit signs each
//! webhook with the API key: the `Authorization` header is a JWT that holds
//! the SHA-256 of the body. `WebhookVerifier` checks it and reduces the event
//! to the signals that end a call.

use livekit_api::access_token::TokenVerifier;
use livekit_api::webhooks::{WebhookError, WebhookReceiver};

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

#[derive(Clone)]
pub struct WebhookVerifier {
    receiver: WebhookReceiver,
}

impl WebhookVerifier {
    pub fn new(api_key: &str, api_secret: &str) -> Self {
        Self {
            receiver: WebhookReceiver::new(TokenVerifier::with_api_key(api_key, api_secret)),
        }
    }

    /// Verify the signature and read the event.
    pub fn verify(&self, body: &str, authorization: &str) -> Result<CallSignal, WebhookError> {
        let event = self.receiver.receive(body, authorization)?;
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
    use base64::Engine;
    use livekit_api::access_token::AccessToken;
    use sha2::{Digest, Sha256};

    const KEY: &str = "devkey";
    const SECRET: &str = "devsecret_devsecret_devsecret_32b";

    /// Sign a body the way LiveKit does.
    fn sign(body: &str, secret: &str) -> String {
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        AccessToken::with_api_key(KEY, secret)
            .with_sha256(&hash)
            .to_jwt()
            .unwrap()
    }

    const LEFT: &str = r#"{"event":"participant_left","room":{"name":"campaign-7"},
        "participant":{"identity":"sip-attempt-9"},"id":"EV_1","createdAt":"1800000000"}"#;

    #[test]
    fn a_signed_participant_left_is_read() {
        let v = WebhookVerifier::new(KEY, SECRET);
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
        let v = WebhookVerifier::new(KEY, SECRET);
        assert_eq!(
            v.verify(body, &sign(body, SECRET)).unwrap(),
            CallSignal::RoomFinished {
                room: "campaign-7".into()
            }
        );
    }

    #[test]
    fn a_wrong_key_or_a_changed_body_is_refused() {
        let v = WebhookVerifier::new(KEY, SECRET);
        let other_secret = "another_secret_another_secret_32";
        assert!(v.verify(LEFT, &sign(LEFT, other_secret)).is_err());
        let changed = LEFT.replace("sip-attempt-9", "sip-attempt-8");
        assert!(v.verify(&changed, &sign(LEFT, SECRET)).is_err());
        assert!(v.verify(LEFT, "not a token").is_err());
    }

    #[test]
    fn other_events_end_nothing() {
        let body = r#"{"event":"track_published","room":{"name":"r"},"id":"EV_3"}"#;
        let v = WebhookVerifier::new(KEY, SECRET);
        assert_eq!(
            v.verify(body, &sign(body, SECRET)).unwrap(),
            CallSignal::Other
        );
    }
}
