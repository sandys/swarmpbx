//! LiveKit tokens and server requests, dated by the role's clock (GAPS.md
//! OPS-9, decision D-11).
//!
//! `livekit-api` dates each token with the OS clock (`SystemTime`), and it
//! gives no way to set `nbf` and `exp`. LiveKit refuses a token whose `nbf` is
//! more than one minute ahead of its own clock, so a node with an OS clock
//! ahead would fail all its calls. `Signer` makes the same tokens (the
//! `livekit-api` claims, HS256) with the times from a `Clock`, and checks the
//! webhook tokens of LiveKit against that clock. `Api` sends the server
//! requests of the dialer with one signed token each.

use std::sync::Arc;
use std::time::Duration;

use ad_core::Clock;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub use livekit_api::access_token::{Claims, VideoGrants};

mod api;
pub use api::{Api, ApiError};

/// The leeway of LiveKit for `nbf` and `exp` (`tokenLeeway` in
/// `livekit/protocol` `auth/verifier.go`).
pub const LEEWAY: Duration = Duration::from_secs(60);
/// The life of a token that signs one server request.
pub const REQUEST_TTL: Duration = Duration::from_secs(600);

const HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the API key or the secret is empty")]
    NoKeys,
    #[error("a token that grants room_join needs an identity and a room")]
    JoinWithoutIdentity,
    #[error("the token is not a signed JWT")]
    Malformed,
    #[error("the signature is wrong")]
    Signature,
    #[error("the token is from another API key")]
    Issuer,
    #[error("the token is expired")]
    Expired,
    #[error("the token is not valid yet")]
    NotYetValid,
    #[error("the body does not match the token")]
    Body,
}

/// Signs and checks LiveKit tokens with one API key, at the time of `clock`.
#[derive(Clone)]
pub struct Signer {
    api_key: String,
    api_secret: String,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("api_key", &self.api_key)
            .finish_non_exhaustive()
    }
}

impl Signer {
    pub fn new(api_key: &str, api_secret: &str, clock: Arc<dyn Clock>) -> Self {
        Self {
            api_key: api_key.to_owned(),
            api_secret: api_secret.to_owned(),
            clock,
        }
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Sign `claims`, valid from now for `ttl`. The signer sets `iss`, `nbf`,
    /// and `exp`.
    pub fn sign(&self, mut claims: Claims, ttl: Duration) -> Result<String, TokenError> {
        if self.api_key.is_empty() || self.api_secret.is_empty() {
            return Err(TokenError::NoKeys);
        }
        if claims.video.room_join && (claims.sub.is_empty() || claims.video.room.is_empty()) {
            return Err(TokenError::JoinWithoutIdentity);
        }
        let now = self.clock.now_utc().timestamp().max(0) as usize;
        claims.iss = self.api_key.clone();
        claims.nbf = now;
        claims.exp = now + ttl.as_secs() as usize;
        let payload = serde_json::to_vec(&claims).map_err(|_| TokenError::Malformed)?;
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(HEADER),
            URL_SAFE_NO_PAD.encode(payload)
        );
        let sig = URL_SAFE_NO_PAD.encode(self.mac(&signed).finalize().into_bytes());
        Ok(format!("{signed}.{sig}"))
    }

    /// A token that lets `identity` join `room` with these grants.
    pub fn join(
        &self,
        room: &str,
        identity: &str,
        kind: Option<&str>,
        grants: VideoGrants,
        ttl: Duration,
    ) -> Result<String, TokenError> {
        self.sign(
            Claims {
                sub: identity.to_owned(),
                name: identity.to_owned(),
                kind: kind.unwrap_or_default().to_owned(),
                video: VideoGrants {
                    room_join: true,
                    room: room.to_owned(),
                    ..grants
                },
                ..Default::default()
            },
            ttl,
        )
    }

    /// Check a token of this API key: the signature, the issuer, and `nbf`
    /// and `exp` against the clock, with `LEEWAY`.
    pub fn verify(&self, token: &str) -> Result<Claims, TokenError> {
        let token = token.strip_prefix("Bearer ").unwrap_or(token);
        let (signed, sig) = token.rsplit_once('.').ok_or(TokenError::Malformed)?;
        let (header, payload) = signed.split_once('.').ok_or(TokenError::Malformed)?;
        let header: serde_json::Value =
            serde_json::from_slice(&decode(header)?).map_err(|_| TokenError::Malformed)?;
        if header.get("alg").and_then(|a| a.as_str()) != Some("HS256") {
            return Err(TokenError::Malformed);
        }
        self.mac(signed)
            .verify_slice(&decode(sig)?)
            .map_err(|_| TokenError::Signature)?;
        let claims: Claims =
            serde_json::from_slice(&decode(payload)?).map_err(|_| TokenError::Malformed)?;
        if claims.iss != self.api_key {
            return Err(TokenError::Issuer);
        }
        let now = self.clock.now_utc().timestamp();
        let leeway = LEEWAY.as_secs() as i64;
        if (claims.exp as i64) + leeway <= now {
            return Err(TokenError::Expired);
        }
        if (claims.nbf as i64) - leeway > now {
            return Err(TokenError::NotYetValid);
        }
        Ok(claims)
    }

    /// Check a webhook: its token, and that the token holds the SHA-256 of
    /// the body.
    pub fn verify_body(&self, body: &str, token: &str) -> Result<Claims, TokenError> {
        let claims = self.verify(token)?;
        let want = STANDARD
            .decode(&claims.sha256)
            .map_err(|_| TokenError::Body)?;
        if want[..] != Sha256::digest(body)[..] {
            return Err(TokenError::Body);
        }
        Ok(claims)
    }

    fn mac(&self, signed: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC takes a key of any length");
        mac.update(signed.as_bytes());
        mac
    }
}

fn decode(part: &str) -> Result<Vec<u8>, TokenError> {
    URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| TokenError::Malformed)
}

#[cfg(test)]
mod tests;
