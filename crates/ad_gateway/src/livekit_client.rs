//! Real LiveKit `GatewayClient`: the room operations of a call (create a
//! room, list who is in it, close it) and the presence check of the
//! reconcile. The call itself is ours (`sip_call`); LiveKit only runs the
//! rooms.

use std::sync::Arc;
use std::time::Duration;

use ad_core::Clock;
use ad_livekit::{Api, Signer};
use livekit_protocol::participant_info::State;
use livekit_protocol::CreateRoomRequest;

use crate::{GatewayClient, GatewayConfig};

/// How long the callee may ring before we CANCEL.
pub const RING_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a dial takes from the INVITE to the final response, with
/// the CANCEL and its 487.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(40);
/// An attempt room with no participant closes after this time.
const ROOM_EMPTY_TIMEOUT_S: u32 = 120;

pub struct LiveKitGateway {
    rooms: Api,
}

impl LiveKitGateway {
    /// The tokens of the requests take their time from `clock` (OPS-9).
    pub fn new(config: &GatewayConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            rooms: Api::new(
                &config.livekit_url,
                Signer::new(&config.api_key, &config.api_secret, clock),
            ),
        }
    }
}

impl GatewayClient for LiveKitGateway {
    async fn participant_present(&self, room: &str, identity: &str) -> Result<bool, String> {
        match self.rooms.get_participant(room, identity).await {
            Ok(p) => Ok(p.state != State::Disconnected as i32),
            Err(e) if e.is_not_found() => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn create_room(&self, room: &str) -> Result<(), String> {
        let req = CreateRoomRequest {
            name: room.to_string(),
            // An attempt room that nobody joins closes by itself.
            empty_timeout: ROOM_EMPTY_TIMEOUT_S,
            ..Default::default()
        };
        self.rooms
            .create_room(req)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn identities(&self, room: &str) -> Result<Vec<String>, String> {
        match self.rooms.list_participants(room).await {
            Ok(list) => Ok(list
                .into_iter()
                .filter(|p| p.state != State::Disconnected as i32)
                .map(|p| p.identity)
                .collect()),
            Err(e) if e.is_not_found() => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn delete_room(&self, room: &str) -> Result<(), String> {
        match self.rooms.delete_room(room).await {
            Ok(()) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}
