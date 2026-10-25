//! `ad_gateway` — the call path of a node (PLAN.md §0.0, §0.9a).
//!
//! - `sip_call`: the node carries the call itself. Our SIP user agent
//!   (`ad_sip`) places it, our RTP leg (`ad_media`) carries its audio, and
//!   our WebRTC participant (`LiveKitParticipant`) brings the audio into the
//!   attempt's LiveKit room, where the agent is.
//! - `telephony`: the providers that a dial goes through: a SIP trunk, or
//!   Twilio click-to-call (`click_to_call`), where Twilio calls the phone and
//!   then calls the node.
//! - `Gateway`: the room operations of a call (create, list, close) and the
//!   presence check of the reconcile, behind a `GatewayClient` trait so unit
//!   tests use a fake.
//! - `webhook`: reads LiveKit webhooks.

mod livekit_client;
pub use livekit_client::{LiveKitGateway, DIAL_TIMEOUT, RING_TIMEOUT};

pub mod webhook;
pub use webhook::{CallSignal, WebhookVerifier};

pub mod bridge;
pub use bridge::{
    AudioFrame, BridgeError, BridgeStats, MediaBridge, RoomConnection, RoomParticipant,
    RoomSession, RtpSink, RtpSource,
};

pub mod click_to_call;
pub mod sip_call;
pub mod telephony;
mod webrtc_participant;
pub use click_to_call::{ClickToCall, TwilioVoice};
pub use sip_call::{CallEnd, Dialed, LiveCall, SipCaller, SipDial, SipTrunk};
pub use telephony::Telephony;
pub use webrtc_participant::{mint_token, LiveKitParticipant, LiveKitSession};

/// How to reach LiveKit.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub livekit_url: String,
    pub api_key: String,
    pub api_secret: String,
    /// Room name prefix: the room of an attempt is `${prefix}-${attempt_id}`.
    pub room_prefix: String,
}

/// The seam to LiveKit.
pub trait GatewayClient: Send + Sync {
    /// Whether the participant is in the room now.
    fn participant_present(
        &self,
        room: &str,
        identity: &str,
    ) -> impl std::future::Future<Output = Result<bool, String>> + Send;

    /// Create the room of an attempt. A room that exists is not an error.
    fn create_room(
        &self,
        room: &str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    /// The identities in the room now.
    fn identities(
        &self,
        room: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, String>> + Send;

    /// Close the room: every participant leaves, and a SIP leg hangs up. A
    /// missing room is not an error.
    fn delete_room(
        &self,
        room: &str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// The room operations over any `GatewayClient`.
pub struct Gateway<C: GatewayClient> {
    client: C,
}

impl<C: GatewayClient> Gateway<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }

    /// Whether the call is still in its room. An error means "unknown".
    pub async fn present(&self, room: &str, identity: &str) -> Result<bool, String> {
        self.client.participant_present(room, identity).await
    }

    pub async fn create_room(&self, room: &str) -> Result<(), String> {
        self.client.create_room(room).await
    }

    /// The first identity in the room other than `except` (the callee): the
    /// agent, before the dial.
    pub async fn other_in_room(&self, room: &str, except: &str) -> Result<Option<String>, String> {
        Ok(self
            .client
            .identities(room)
            .await?
            .into_iter()
            .find(|i| i != except))
    }

    pub async fn delete_room(&self, room: &str) -> Result<(), String> {
        self.client.delete_room(room).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake client with programmed room contents.
    struct FakeClient {
        present: Result<bool, String>,
        in_room: Vec<String>,
    }

    impl FakeClient {
        fn new() -> Self {
            FakeClient {
                present: Ok(true),
                in_room: Vec::new(),
            }
        }
    }

    impl GatewayClient for FakeClient {
        async fn participant_present(&self, _room: &str, _identity: &str) -> Result<bool, String> {
            self.present.clone()
        }

        async fn create_room(&self, _room: &str) -> Result<(), String> {
            Ok(())
        }

        async fn identities(&self, _room: &str) -> Result<Vec<String>, String> {
            Ok(self.in_room.clone())
        }

        async fn delete_room(&self, _room: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_agent_is_the_other_identity_in_the_room() {
        let mut client = FakeClient::new();
        client.in_room = vec!["sip-attempt-7".into(), "agent-AJ_1".into()];
        let gw = Gateway::new(client);
        assert_eq!(
            gw.other_in_room("r", "sip-attempt-7")
                .await
                .unwrap()
                .as_deref(),
            Some("agent-AJ_1")
        );
        let gw = Gateway::new(FakeClient::new());
        assert_eq!(gw.other_in_room("r", "sip-attempt-7").await.unwrap(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn presence_errors_mean_unknown() {
        let mut client = FakeClient::new();
        client.present = Err("down".into());
        assert!(Gateway::new(client).present("r", "i").await.is_err());
    }
}
