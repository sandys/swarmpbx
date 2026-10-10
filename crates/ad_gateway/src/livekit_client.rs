//! Real LiveKit `GatewayClient` (feature `livekit`). Originates an outbound SIP
//! call into a LiveKit room via `SIPClient::create_sip_participant`. The media
//! networking (SIP/RTP) runs in the LiveKit SIP pods; this is the control call.

use livekit_api::services::sip::{CreateSIPParticipantOptions, SIPClient};

use crate::{GatewayClient, GatewayConfig, Origination};

pub struct LiveKitGateway {
    client: SIPClient,
    sip_trunk_id: String,
}

impl LiveKitGateway {
    pub fn new(config: &GatewayConfig) -> Self {
        let client =
            SIPClient::with_api_key(&config.livekit_url, &config.api_key, &config.api_secret);
        Self {
            client,
            sip_trunk_id: config.sip_trunk_id.clone(),
        }
    }
}

impl GatewayClient for LiveKitGateway {
    async fn create_sip_participant(
        &self,
        room: &str,
        phone_number: &str,
    ) -> Result<Origination, String> {
        let identity = format!("sip_{phone_number}");
        let options = CreateSIPParticipantOptions {
            participant_identity: identity.clone(),
            // Block until the callee answers so the outcome reflects a real
            // connection, mapping SIP error codes to a failure.
            wait_until_answered: Some(true),
            ..Default::default()
        };
        self.client
            .create_sip_participant(
                self.sip_trunk_id.clone(),
                phone_number.to_string(),
                room.to_string(),
                options,
                None,
            )
            .await
            .map(|_info| Origination {
                room: room.to_string(),
                participant_identity: identity,
            })
            .map_err(|e| e.to_string())
    }
}
