//! The LiveKit server requests of the dialer (Twirp: a protobuf body in a
//! POST to `/twirp/livekit.<Service>/<Method>`), each with a token from the
//! `Signer`. One HTTP client keeps the connections. Unlike `livekit-api`,
//! this client does not fail over to another LiveKit Cloud region.

use std::time::Duration;

use livekit_protocol as proto;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::Deserialize;

use crate::{Signer, TokenError, VideoGrants, REQUEST_TTL};

/// The time limit of one request, as in `livekit-api`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("the request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("LiveKit answered {status} {code}: {msg}")]
    Server {
        status: u16,
        code: String,
        msg: String,
    },
    #[error("the answer does not decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("token: {0}")]
    Token(#[from] TokenError),
}

impl ApiError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, ApiError::Server { code, .. } if code == "not_found")
    }
}

#[derive(Deserialize)]
struct TwirpError {
    code: String,
    #[serde(default)]
    msg: String,
}

/// The server API of one LiveKit server.
#[derive(Debug, Clone)]
pub struct Api {
    base: String,
    http: reqwest::Client,
    signer: Signer,
}

impl Api {
    /// `url` is the LiveKit URL; `ws://` and `wss://` become `http://` and
    /// `https://`.
    pub fn new(url: &str, signer: Signer) -> Self {
        let base = if let Some(rest) = url.strip_prefix("wss://") {
            format!("https://{rest}")
        } else if let Some(rest) = url.strip_prefix("ws://") {
            format!("http://{rest}")
        } else {
            url.to_owned()
        };
        Self {
            base: base.trim_end_matches('/').to_owned(),
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("an HTTP client with a timeout builds"),
            signer,
        }
    }

    pub fn signer(&self) -> &Signer {
        &self.signer
    }

    async fn call<D: prost::Message, R: prost::Message + Default>(
        &self,
        service: &str,
        method: &str,
        grants: VideoGrants,
        req: D,
    ) -> Result<R, ApiError> {
        let token = self.signer.sign(
            crate::Claims {
                video: grants,
                ..Default::default()
            },
            REQUEST_TTL,
        )?;
        let resp = self
            .http
            .post(format!("{}/twirp/livekit.{service}/{method}", self.base))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/protobuf")
            .header(USER_AGENT, "agentdialer")
            .body(req.encode_to_vec())
            .send()
            .await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        if status.is_success() {
            return Ok(R::decode(body)?);
        }
        let (code, msg) = match serde_json::from_slice::<TwirpError>(&body) {
            Ok(e) => (e.code, e.msg),
            Err(_) => (
                "unknown".to_owned(),
                String::from_utf8_lossy(&body).into_owned(),
            ),
        };
        Err(ApiError::Server {
            status: status.as_u16(),
            code,
            msg,
        })
    }

    fn admin(room: &str) -> VideoGrants {
        VideoGrants {
            room_admin: true,
            room: room.to_owned(),
            ..Default::default()
        }
    }

    pub async fn create_room(
        &self,
        req: proto::CreateRoomRequest,
    ) -> Result<proto::Room, ApiError> {
        let grants = VideoGrants {
            room_create: true,
            ..Default::default()
        };
        self.call("RoomService", "CreateRoom", grants, req).await
    }

    pub async fn delete_room(&self, room: &str) -> Result<(), ApiError> {
        let grants = VideoGrants {
            room_create: true,
            ..Default::default()
        };
        let req = proto::DeleteRoomRequest {
            room: room.to_owned(),
        };
        self.call::<_, proto::DeleteRoomResponse>("RoomService", "DeleteRoom", grants, req)
            .await
            .map(|_| ())
    }

    pub async fn list_participants(
        &self,
        room: &str,
    ) -> Result<Vec<proto::ParticipantInfo>, ApiError> {
        let req = proto::ListParticipantsRequest {
            room: room.to_owned(),
        };
        self.call::<_, proto::ListParticipantsResponse>(
            "RoomService",
            "ListParticipants",
            Self::admin(room),
            req,
        )
        .await
        .map(|r| r.participants)
    }

    pub async fn get_participant(
        &self,
        room: &str,
        identity: &str,
    ) -> Result<proto::ParticipantInfo, ApiError> {
        let req = proto::RoomParticipantIdentity {
            room: room.to_owned(),
            identity: identity.to_owned(),
            ..Default::default()
        };
        self.call("RoomService", "GetParticipant", Self::admin(room), req)
            .await
    }

    pub async fn create_dispatch(
        &self,
        req: proto::CreateAgentDispatchRequest,
    ) -> Result<proto::AgentDispatch, ApiError> {
        let grants = Self::admin(&req.room);
        self.call("AgentDispatchService", "CreateDispatch", grants, req)
            .await
    }
}
