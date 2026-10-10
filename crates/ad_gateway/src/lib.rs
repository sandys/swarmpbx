//! `ad_gateway` — the real `CallInitiator` (PLAN.md §0, §0.9a). It originates a
//! call by creating/using a LiveKit room and placing a SIP participant into it
//! (`CreateSIPParticipant`), so an answered leg lands in the room where the
//! agent subscribes. This is the concrete implementation behind the dispatch
//! seam; the stub used in earlier slices is replaced by this.
//!
//! The LiveKit calls sit behind a `GatewayClient` trait so the crate and the
//! fast test tier build without the SDK; the real client is feature-gated
//! (`--features livekit`) and used by the orchestrator binary. The media
//! networking itself (drachtio + rtpengine, or LiveKit SIP) runs as separate
//! pods; `ad_gateway` is the control-plane originator.

use ad_dispatch::{CallInitiator, CallOutcome};
use ad_lead::Lead;

#[cfg(feature = "livekit")]
mod livekit_client;
#[cfg(feature = "livekit")]
pub use livekit_client::LiveKitGateway;

pub mod bridge;
pub use bridge::{
    AudioFrame, BridgeError, BridgeStats, MediaBridge, RoomConnection, RoomParticipant,
    RoomSession, RtpSink, RtpSource,
};

#[cfg(feature = "webrtc")]
mod webrtc_participant;
#[cfg(feature = "webrtc")]
pub use webrtc_participant::LiveKitParticipant;

/// Config to reach LiveKit and the SIP trunk used for origination.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub livekit_url: String,
    pub api_key: String,
    pub api_secret: String,
    /// The SIP trunk id to originate outbound calls through.
    pub sip_trunk_id: String,
    /// Room name prefix; one room per call: `${prefix}-${lead_id}`.
    pub room_prefix: String,
}

/// What a successful origination produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origination {
    pub room: String,
    pub participant_identity: String,
}

/// The seam to LiveKit. `create_sip_participant` must create (or reuse) the room
/// and originate the outbound SIP call into it, returning the participant.
pub trait GatewayClient: Send + Sync {
    fn create_sip_participant(
        &self,
        room: &str,
        phone_number: &str,
    ) -> impl std::future::Future<Output = Result<Origination, String>> + Send;
}

/// The gateway: a `CallInitiator` built over any `GatewayClient`.
pub struct Gateway<C: GatewayClient> {
    client: C,
    room_prefix: String,
}

impl<C: GatewayClient> Gateway<C> {
    pub fn new(client: C, room_prefix: impl Into<String>) -> Self {
        Self {
            client,
            room_prefix: room_prefix.into(),
        }
    }

    fn room_for(&self, lead: &Lead) -> String {
        format!("{}-{}", self.room_prefix, lead.id.0)
    }
}

impl<C: GatewayClient> CallInitiator for Gateway<C> {
    async fn initiate(&self, lead: &Lead) -> CallOutcome {
        let room = self.room_for(lead);
        match self
            .client
            .create_sip_participant(&room, &lead.phone_number)
            .await
        {
            // The participant was created and the SIP leg originated. Whether
            // the callee ultimately answers is observed later via call events;
            // for the origination step, a created participant is "placed".
            Ok(_o) => CallOutcome::Answered,
            Err(e) => CallOutcome::Failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use ad_core::{CampaignId, LeadId};
    use ad_lead::{Lead, LeadStatus};

    /// A fake client recording calls; programmable to succeed or fail.
    struct FakeClient {
        calls: Arc<AtomicUsize>,
        fail: Option<String>,
        last_room: std::sync::Mutex<Option<String>>,
        last_phone: std::sync::Mutex<Option<String>>,
    }
    impl GatewayClient for FakeClient {
        async fn create_sip_participant(
            &self,
            room: &str,
            phone: &str,
        ) -> Result<Origination, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_room.lock().unwrap() = Some(room.to_string());
            *self.last_phone.lock().unwrap() = Some(phone.to_string());
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(Origination {
                    room: room.to_string(),
                    participant_identity: format!("sip_{phone}"),
                }),
            }
        }
    }

    fn lead(id: i64, phone: &str) -> Lead {
        let t = ad_core::time::SystemClock;
        use ad_core::time::Clock;
        let now = t.now_utc();
        Lead {
            id: LeadId(id),
            campaign_id: CampaignId(1),
            phone_number: phone.into(),
            timezone: Some("America/New_York".into()),
            state: None,
            postal_code: None,
            status: LeadStatus::Queued,
            called_count: 0,
            last_local_call_time: None,
            context: BTreeMap::new(),
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initiate_creates_sip_participant_in_per_lead_room() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = FakeClient {
            calls: calls.clone(),
            fail: None,
            last_room: Default::default(),
            last_phone: Default::default(),
        };
        let gw = Gateway::new(client, "campaign-7");
        let out = gw.initiate(&lead(42, "+13125550100")).await;
        assert_eq!(out, CallOutcome::Answered);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            gw.client.last_room.lock().unwrap().as_deref(),
            Some("campaign-7-42")
        );
        assert_eq!(
            gw.client.last_phone.lock().unwrap().as_deref(),
            Some("+13125550100")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initiate_maps_client_error_to_failed_outcome() {
        let client = FakeClient {
            calls: Arc::new(AtomicUsize::new(0)),
            fail: Some("trunk unreachable".into()),
            last_room: Default::default(),
            last_phone: Default::default(),
        };
        let gw = Gateway::new(client, "c");
        let out = gw.initiate(&lead(1, "+1")).await;
        assert_eq!(out, CallOutcome::Failed("trunk unreachable".into()));
    }
}
