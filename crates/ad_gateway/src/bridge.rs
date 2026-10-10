//! The SIP↔WebRTC bridge seam (PLAN.md §0.9a, option 2). We own the media
//! translation: a native WebRTC participant joins a LiveKit room, publishes an
//! outbound audio track fed from the SIP/RTP side, and subscribes to the agent's
//! track to feed audio back to RTP. No dependency on `livekit/sip`.
//!
//! Everything here is defined over traits so the fast test tier runs with
//! in-memory fakes. The real libwebrtc-backed participant is feature-gated
//! (`webrtc`) and lives in `webrtc_participant.rs`.

use std::future::Future;

use tokio::sync::mpsc;

/// A chunk of linear PCM audio. 16-bit signed, interleaved by channel. This is
/// the common currency between the RTP side (after rtpengine transcode) and the
/// WebRTC side (libwebrtc native frames).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u32,
    /// Samples per channel in this frame (`samples.len() / channels`).
    pub samples_per_channel: usize,
}

impl AudioFrame {
    pub fn new(samples: Vec<i16>, sample_rate: u32, channels: u32) -> Self {
        let spc = if channels == 0 {
            0
        } else {
            samples.len() / channels as usize
        };
        Self {
            samples,
            sample_rate,
            channels,
            samples_per_channel: spc,
        }
    }

    /// A silent frame of `samples_per_channel` samples (used to prime a track).
    pub fn silence(sample_rate: u32, channels: u32, samples_per_channel: usize) -> Self {
        let len = samples_per_channel * channels as usize;
        Self {
            samples: vec![0; len],
            sample_rate,
            channels,
            samples_per_channel,
        }
    }
}

/// Errors from the bridge or a participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    Join(String),
    Publish(String),
    Subscribe(String),
    Transport(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Join(e) => write!(f, "room join failed: {e}"),
            BridgeError::Publish(e) => write!(f, "publish failed: {e}"),
            BridgeError::Subscribe(e) => write!(f, "subscribe failed: {e}"),
            BridgeError::Transport(e) => write!(f, "transport error: {e}"),
        }
    }
}
impl std::error::Error for BridgeError {}

/// How to reach + authenticate to the room.
#[derive(Debug, Clone)]
pub struct RoomConnection {
    pub url: String,
    /// A room-join access token for the participant identity.
    pub token: String,
    pub room: String,
    pub identity: String,
}

/// A joined room session. Outbound audio is written with `send_audio`; inbound
/// audio (the agent's track) arrives on the receiver from `take_inbound`.
pub trait RoomSession: Send {
    /// Push one PCM frame to the published (outbound) track.
    fn send_audio(&self, frame: AudioFrame)
        -> impl Future<Output = Result<(), BridgeError>> + Send;

    /// Take the inbound audio receiver exactly once. Frames are the agent's
    /// subscribed track, decoded to PCM.
    fn take_inbound(&mut self) -> Option<mpsc::Receiver<AudioFrame>>;

    /// Leave the room and release the tracks.
    fn close(self) -> impl Future<Output = ()> + Send;
}

/// Joins a LiveKit room as a native WebRTC participant.
pub trait RoomParticipant: Send + Sync {
    type Session: RoomSession;
    fn join(
        &self,
        conn: RoomConnection,
    ) -> impl Future<Output = Result<Self::Session, BridgeError>> + Send;
}

/// The SIP/RTP side. `recv` yields PCM frames arriving from the caller (after
/// rtpengine transcode to linear PCM); `send` writes PCM frames back toward the
/// caller.
pub trait RtpSource: Send {
    fn recv(&mut self) -> impl Future<Output = Option<AudioFrame>> + Send;
}
pub trait RtpSink: Send {
    fn send(&self, frame: AudioFrame) -> impl Future<Output = Result<(), BridgeError>> + Send;
}

/// Pumps audio both ways between the RTP side and the room session:
///   caller RTP  ──(RtpSource)──►  room outbound track   (send_audio)
///   agent track ──(take_inbound)─►  caller RTP           (RtpSink.send)
///
/// Runs until either side ends. Returns the per-direction frame counts, which
/// the e2e asserts are non-zero in both directions.
pub struct MediaBridge;

/// Frames carried in each direction during a bridge run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeStats {
    pub outbound_frames: u64,
    pub inbound_frames: u64,
}

impl MediaBridge {
    /// Run the two pumps concurrently until both directions finish.
    pub async fn run<S, Src, Sink>(
        mut session: S,
        mut rtp_in: Src,
        rtp_out: Sink,
    ) -> Result<BridgeStats, BridgeError>
    where
        S: RoomSession,
        Src: RtpSource,
        Sink: RtpSink,
    {
        let mut inbound_rx = session
            .take_inbound()
            .ok_or_else(|| BridgeError::Subscribe("inbound receiver already taken".into()))?;

        let mut stats = BridgeStats::default();

        // Outbound: drain the RTP source into the room's published track.
        // Inbound: drain the agent's subscribed track into the RTP sink.
        // Both run until their source ends; we interleave with select.
        let mut rtp_open = true;
        let mut room_open = true;
        while rtp_open || room_open {
            tokio::select! {
                maybe_rtp = rtp_in.recv(), if rtp_open => {
                    match maybe_rtp {
                        Some(frame) => {
                            session.send_audio(frame).await?;
                            stats.outbound_frames += 1;
                        }
                        None => rtp_open = false,
                    }
                }
                maybe_room = inbound_rx.recv(), if room_open => {
                    match maybe_room {
                        Some(frame) => {
                            rtp_out.send(frame).await?;
                            stats.inbound_frames += 1;
                        }
                        None => room_open = false,
                    }
                }
            }
        }

        session.close().await;
        Ok(stats)
    }
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod bridge_tests;
