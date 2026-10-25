//! The real native WebRTC participant (PLAN.md §0.9a option 2). It joins a
//! LiveKit room via the Rust SDK, publishes an outbound audio track fed by
//! `send_audio`, and forwards the agent's subscribed track into an inbound
//! channel. This is OUR SIP↔WebRTC media leg — no `livekit/sip`.

use futures_util::StreamExt;
use livekit::options::TrackPublishOptions;
use livekit::track::{LocalAudioTrack, LocalTrack, RemoteTrack, TrackSource};
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit::webrtc::prelude::AudioFrame as WebRtcAudioFrame;
use livekit::{Room, RoomEvent, RoomOptions};
use livekit_api::access_token::{AccessToken, VideoGrants};
use tokio::sync::mpsc;

use crate::bridge::{AudioFrame, BridgeError, RoomConnection, RoomParticipant, RoomSession};

/// Mint a room-join token for a participant identity with publish + subscribe.
pub fn mint_token(
    api_key: &str,
    api_secret: &str,
    room: &str,
    identity: &str,
) -> Result<String, BridgeError> {
    AccessToken::with_api_key(api_key, api_secret)
        .with_identity(identity)
        .with_name(identity)
        .with_grants(VideoGrants {
            room_join: true,
            room: room.to_string(),
            can_publish: Some(true),
            can_subscribe: Some(true),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|e| BridgeError::Join(e.to_string()))
}

/// Joins LiveKit rooms as a native WebRTC participant.
pub struct LiveKitParticipant {
    pub sample_rate: u32,
    pub channels: u32,
}

impl Default for LiveKitParticipant {
    fn default() -> Self {
        // 48 kHz mono is the WebRTC-native rate; rtpengine transcodes 8 kHz
        // telephony PCM up to this on the way in.
        Self {
            sample_rate: 48000,
            channels: 1,
        }
    }
}

/// A live room session backed by the SDK.
pub struct LiveKitSession {
    room: Room,
    source: NativeAudioSource,
    sample_rate: u32,
    channels: u32,
    inbound: Option<mpsc::Receiver<AudioFrame>>,
    others_left: tokio::sync::watch::Receiver<bool>,
    others_joined: tokio::sync::watch::Receiver<Option<String>>,
    _forward_task: tokio::task::JoinHandle<()>,
}

impl LiveKitSession {
    /// Turns true when another participant leaves the room. A room holds one
    /// call, so that participant is the agent.
    pub fn others_left(&self) -> tokio::sync::watch::Receiver<bool> {
        self.others_left.clone()
    }

    /// The identity of the first other participant (the agent) in the room,
    /// once one is there.
    pub fn others_joined(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.others_joined.clone()
    }
}

impl RoomParticipant for LiveKitParticipant {
    type Session = LiveKitSession;

    async fn join(&self, conn: RoomConnection) -> Result<Self::Session, BridgeError> {
        let (room, mut events) = Room::connect(&conn.url, &conn.token, RoomOptions::default())
            .await
            .map_err(|e| BridgeError::Join(e.to_string()))?;

        // Publish an outbound audio track fed from a native source.
        let source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            self.sample_rate,
            self.channels,
            1000,
        );
        let track = LocalAudioTrack::create_audio_track(
            "sip-outbound",
            RtcAudioSource::Native(source.clone()),
        );
        room.local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| BridgeError::Publish(e.to_string()))?;

        // Forward each subscribed remote audio track (the agent) into a
        // channel the bridge drains, at our sample rate (libwebrtc resamples).
        // Report the first participant that joins, and a participant that
        // leaves.
        let (in_tx, in_rx) = mpsc::channel::<AudioFrame>(256);
        let (left_tx, others_left) = tokio::sync::watch::channel(false);
        let first = room.remote_participants().into_keys().next().map(|id| id.0);
        let (joined_tx, others_joined) = tokio::sync::watch::channel(first);
        let (rate, channels) = (self.sample_rate, self.channels);
        let forward_task = tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    RoomEvent::TrackSubscribed {
                        track: RemoteTrack::Audio(audio),
                        ..
                    } => {
                        let in_tx = in_tx.clone();
                        tokio::spawn(async move {
                            let mut stream = NativeAudioStream::new(
                                audio.rtc_track(),
                                rate as i32,
                                channels as i32,
                            );
                            while let Some(frame) = stream.next().await {
                                let pcm = AudioFrame::new(
                                    frame.data.to_vec(),
                                    frame.sample_rate,
                                    frame.num_channels,
                                );
                                if in_tx.send(pcm).await.is_err() {
                                    return; // the bridge dropped the receiver
                                }
                            }
                        });
                    }
                    RoomEvent::ParticipantConnected(p) => {
                        joined_tx.send_if_modified(|joined| {
                            if joined.is_some() {
                                return false;
                            }
                            *joined = Some(p.identity().0);
                            true
                        });
                    }
                    RoomEvent::ParticipantDisconnected(_) => {
                        let _ = left_tx.send(true);
                    }
                    _ => {}
                }
            }
        });

        Ok(LiveKitSession {
            room,
            source,
            sample_rate: self.sample_rate,
            channels: self.channels,
            inbound: Some(in_rx),
            others_left,
            others_joined,
            _forward_task: forward_task,
        })
    }
}

impl RoomSession for LiveKitSession {
    async fn send_audio(&self, frame: AudioFrame) -> Result<(), BridgeError> {
        let wf = WebRtcAudioFrame {
            data: frame.samples.into(),
            sample_rate: self.sample_rate,
            num_channels: self.channels,
            samples_per_channel: (frame.samples_per_channel as u32).max(1),
        };
        self.source
            .capture_frame(&wf)
            .await
            .map_err(|e| BridgeError::Publish(e.to_string()))
    }

    fn take_inbound(&mut self) -> Option<mpsc::Receiver<AudioFrame>> {
        self.inbound.take()
    }

    async fn close(self) {
        let _ = self.room.close().await;
    }
}
