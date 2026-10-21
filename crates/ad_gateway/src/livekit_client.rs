//! Real LiveKit `GatewayClient`: the room operations of a call (create a
//! room, list who is in it, close it) and the presence check of the
//! reconcile. The call itself is ours (`sip_call`); LiveKit only runs the
//! rooms.

use std::time::Duration;

use livekit_api::services::room::{CreateRoomOptions, RoomClient};
use livekit_api::services::{ServerError, ServerErrorCode, ServiceError};
use livekit_protocol::participant_info::State;

use crate::{GatewayClient, GatewayConfig};

/// How long the callee may ring before we CANCEL.
pub const RING_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest a dial takes from the INVITE to the final response, with
/// the CANCEL and its 487.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(40);
/// An attempt room with no participant closes after this time.
const ROOM_EMPTY_TIMEOUT_S: u32 = 120;

pub struct LiveKitGateway {
    rooms: RoomClient,
}

impl LiveKitGateway {
    pub fn new(config: &GatewayConfig) -> Self {
        Self {
            rooms: RoomClient::with_api_key(
                &config.livekit_url,
                &config.api_key,
                &config.api_secret,
            ),
        }
    }
}

impl GatewayClient for LiveKitGateway {
    async fn participant_present(&self, room: &str, identity: &str) -> Result<bool, String> {
        match self.rooms.get_participant(room, identity).await {
            Ok(p) => Ok(p.state != State::Disconnected as i32),
            Err(ServiceError::Twirp(ServerError::Twirp(code)))
                if code.code == ServerErrorCode::NOT_FOUND =>
            {
                Ok(false)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    async fn create_room(&self, room: &str) -> Result<(), String> {
        let options = CreateRoomOptions {
            // An attempt room that nobody joins closes by itself.
            empty_timeout: ROOM_EMPTY_TIMEOUT_S,
            ..Default::default()
        };
        self.rooms
            .create_room(room, options)
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
            Err(ServiceError::Twirp(ServerError::Twirp(code)))
                if code.code == ServerErrorCode::NOT_FOUND =>
            {
                Ok(Vec::new())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    async fn delete_room(&self, room: &str) -> Result<(), String> {
        match self.rooms.delete_room(room).await {
            Ok(()) => Ok(()),
            Err(ServiceError::Twirp(ServerError::Twirp(code)))
                if code.code == ServerErrorCode::NOT_FOUND =>
            {
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}
