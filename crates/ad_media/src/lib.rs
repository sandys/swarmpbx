//! One call's RTP leg (PLAN.md §0.0, decision D-9).
//!
//! The vendored `rustrtc` makes the SDP offer, applies the answer, and carries
//! RTP with symmetric RTP and latching, one UDP socket per call. The vendored
//! `audio-codec` does G.711. This crate adds what the carriers need from us:
//! a 20 ms send clock that sends a packet every tick from the answer to the
//! end (silence when nothing is queued, so the NAT mapping stays open and the
//! carrier can latch), and a media timeout that ends a dead call.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use audio_codec::{create_decoder, create_encoder, CodecType};
use rustrtc::media::{sample_track, AudioFrame, AudioStreamTrack, MediaKind, MediaSample};
use rustrtc::{
    AudioCapability, MediaCapabilities, PeerConnection, PeerConnectionEvent, RtcConfiguration,
    RtpCodecParameters, RtpSender, SdpCompatibilityMode, SdpType, SessionDescription,
    TransportMode,
};
use tokio::sync::mpsc;

/// The telephone clock rate.
pub const SAMPLE_RATE: u32 = 8000;
/// 20 ms of 8 kHz audio: the packet size we send.
pub const FRAME_SAMPLES: usize = 160;
const FRAME: Duration = Duration::from_millis(20);
/// No RTP for this long after the answer means the call is dead.
pub const MEDIA_TIMEOUT: Duration = Duration::from_secs(30);
/// Frames queued toward the callee before the oldest is dropped.
const SEND_QUEUE: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("rtp: {0}")]
    Rtp(String),
    #[error("bad SDP: {0}")]
    Sdp(String),
    #[error("the answer has no codec that we offered: {0}")]
    NoCodec(String),
}

fn rtp<E: std::fmt::Display>(e: E) -> MediaError {
    MediaError::Rtp(e.to_string())
}

/// Where the RTP socket binds and which address the SDP names.
#[derive(Debug, Clone)]
pub struct MediaConfig {
    /// The local address of the RTP socket. The SDP names it unless
    /// `external_ip` is set.
    pub bind_ip: IpAddr,
    /// A public address to name in the SDP (`public` media mode).
    pub external_ip: Option<IpAddr>,
    /// The time without RTP that ends the call.
    pub timeout: Duration,
}

impl MediaConfig {
    pub fn new(bind_ip: IpAddr) -> Self {
        Self {
            bind_ip,
            external_ip: None,
            timeout: MEDIA_TIMEOUT,
        }
    }
}

/// A G.711 codec that both sides support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Pcmu,
    Pcma,
}

impl Codec {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "PCMU" => Some(Codec::Pcmu),
            "PCMA" => Some(Codec::Pcma),
            _ => None,
        }
    }

    fn from_payload_type(pt: u8) -> Option<Self> {
        match pt {
            0 => Some(Codec::Pcmu),
            8 => Some(Codec::Pcma),
            _ => None,
        }
    }

    fn codec_type(self) -> CodecType {
        match self {
            Codec::Pcmu => CodecType::PCMU,
            Codec::Pcma => CodecType::PCMA,
        }
    }
}

fn configuration(cfg: &MediaConfig) -> RtcConfiguration {
    RtcConfiguration {
        transport_mode: TransportMode::Rtp,
        sdp_compatibility: SdpCompatibilityMode::LegacySip,
        bind_ip: Some(cfg.bind_ip.to_string()),
        external_ip: cfg.external_ip.map(|ip| ip.to_string()),
        enable_latching: true,
        media_capabilities: Some(MediaCapabilities {
            audio: vec![
                AudioCapability::pcmu(),
                AudioCapability::pcma(),
                AudioCapability::telephone_event(),
            ],
            video: vec![],
            application: None,
            image: vec![],
        }),
        runtime_handle: tokio::runtime::Handle::try_current().ok(),
        ..Default::default()
    }
}

/// An RTP leg before the call is answered.
pub struct RtpLeg {
    pc: PeerConnection,
    sender: Arc<RtpSender>,
    source: rustrtc::media::SampleStreamSource,
    timeout: Duration,
}

impl RtpLeg {
    /// A leg on `pc`. An answerer must set the remote offer on `pc` first:
    /// `add_track` then reuses the offer's transceiver, and the answer follows
    /// the offer's codecs.
    fn with_track(pc: PeerConnection, cfg: &MediaConfig) -> Result<Self, MediaError> {
        let (source, track, _feedback) = sample_track(MediaKind::Audio, SEND_QUEUE);
        let sender = pc
            .add_track(
                track,
                RtpCodecParameters {
                    payload_type: 0,
                    name: "PCMU".into(),
                    clock_rate: SAMPLE_RATE,
                    channels: 1,
                },
            )
            .map_err(rtp)?;
        Ok(Self {
            pc,
            sender,
            source,
            timeout: cfg.timeout,
        })
    }

    /// A leg and its SDP offer, for the INVITE.
    pub async fn offer(cfg: &MediaConfig) -> Result<(Self, String), MediaError> {
        let leg = Self::with_track(PeerConnection::new(configuration(cfg)), cfg)?;
        // The first offer starts the socket bind; the second names its address.
        let _ = leg.pc.create_offer().await.map_err(rtp)?;
        leg.pc.wait_for_gathering_complete().await;
        let offer = leg.pc.create_offer().await.map_err(rtp)?;
        leg.pc.set_local_description(offer.clone()).map_err(rtp)?;
        Ok((leg, offer.to_sdp_string()))
    }

    /// A leg that answers `remote` (the callee side, for tests and inbound).
    pub async fn answer(cfg: &MediaConfig, remote: &str) -> Result<(Self, String), MediaError> {
        let pc = PeerConnection::new(configuration(cfg));
        let offer = SessionDescription::parse(SdpType::Offer, remote)
            .map_err(|e| MediaError::Sdp(e.to_string()))?;
        pc.set_remote_description(offer).await.map_err(rtp)?;
        let leg = Self::with_track(pc, cfg)?;
        let _ = leg.pc.create_answer().await.map_err(rtp)?;
        leg.pc.wait_for_gathering_complete().await;
        let answer = leg.pc.create_answer().await.map_err(rtp)?;
        leg.pc.set_local_description(answer.clone()).map_err(rtp)?;
        Ok((leg, answer.to_sdp_string()))
    }

    /// Apply the callee's SDP answer from the 2xx (or a 183 with SDP).
    pub async fn accept(&self, answer: &str) -> Result<(), MediaError> {
        let answer = SessionDescription::parse(SdpType::Answer, answer)
            .map_err(|e| MediaError::Sdp(e.to_string()))?;
        self.pc.set_remote_description(answer).await.map_err(rtp)
    }

    /// The codec that the negotiation chose for what we send.
    pub fn codec(&self) -> Result<Codec, MediaError> {
        let params = self.sender.params();
        Codec::from_name(&params.name).ok_or(MediaError::NoCodec(params.name))
    }

    /// Start the media: the send clock and the receive loop.
    pub fn start(self) -> Result<Media, MediaError> {
        let codec = self.codec()?;
        let (to_callee, queue) = mpsc::channel::<Vec<i16>>(SEND_QUEUE);
        let (from_callee, inbound) = mpsc::channel::<Vec<i16>>(SEND_QUEUE);
        let send = tokio::spawn(send_clock(self.source, codec, queue));
        let recv = tokio::spawn(receive(self.pc.clone(), from_callee, self.timeout));
        Ok(Media {
            pc: self.pc,
            codec,
            to_callee,
            inbound: Some(inbound),
            _send: AbortOnDrop(send),
            _recv: AbortOnDrop(recv),
        })
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A running leg. Dropping it stops the media and closes the socket.
pub struct Media {
    pc: PeerConnection,
    codec: Codec,
    to_callee: mpsc::Sender<Vec<i16>>,
    inbound: Option<mpsc::Receiver<Vec<i16>>>,
    _send: AbortOnDrop,
    _recv: AbortOnDrop,
}

impl Media {
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Queue 8 kHz PCM toward the callee. The send clock takes 160 samples
    /// every 20 ms. A full queue drops this audio rather than add delay.
    pub fn send(&self, samples: Vec<i16>) {
        let _ = self.to_callee.try_send(samples);
    }

    /// A clone of the sending side, for a task that feeds the callee.
    pub fn sender(&self) -> MediaOut {
        MediaOut(self.to_callee.clone())
    }

    /// The callee's audio as 8 kHz PCM, 20 ms per item. The channel closes
    /// when no RTP arrives for the media timeout. Take it once.
    pub fn take_inbound(&mut self) -> Option<mpsc::Receiver<Vec<i16>>> {
        self.inbound.take()
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.pc.close();
    }
}

/// The sending side of a leg.
#[derive(Clone)]
pub struct MediaOut(mpsc::Sender<Vec<i16>>);

impl MediaOut {
    pub fn send(&self, samples: Vec<i16>) {
        let _ = self.0.try_send(samples);
    }
}

/// Send one packet every 20 ms: queued audio, else silence. Audio arrives in
/// any size; the clock cuts it into 160-sample packets.
async fn send_clock(
    source: rustrtc::media::SampleStreamSource,
    codec: Codec,
    mut queue: mpsc::Receiver<Vec<i16>>,
) {
    let mut encoder = create_encoder(codec.codec_type());
    let mut pending: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 4);
    let mut tick = tokio::time::interval(FRAME);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut timestamp: u32 = 0;
    let mut first = true;
    loop {
        tick.tick().await;
        while pending.len() < FRAME_SAMPLES {
            match queue.try_recv() {
                Ok(samples) => pending.extend_from_slice(&samples),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return,
            }
        }
        let frame: Vec<i16> = if pending.len() >= FRAME_SAMPLES {
            pending.drain(..FRAME_SAMPLES).collect()
        } else {
            pending.clear();
            vec![0; FRAME_SAMPLES]
        };
        let sample = MediaSample::Audio(AudioFrame {
            rtp_timestamp: timestamp,
            clock_rate: SAMPLE_RATE,
            data: encoder.encode(&frame).into(),
            marker: first,
            ..Default::default()
        });
        first = false;
        timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
        if source.send(sample).is_err() {
            return;
        }
    }
}

/// Decode the callee's RTP to PCM by each packet's payload type. Stop when
/// no RTP arrives for `timeout`.
async fn receive(pc: PeerConnection, out: mpsc::Sender<Vec<i16>>, timeout: Duration) {
    let track = loop {
        match tokio::time::timeout(timeout, pc.recv()).await {
            Ok(Some(PeerConnectionEvent::Track(transceiver))) => {
                if let Some(receiver) = transceiver.receiver() {
                    break receiver.track();
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(_) => {
                tracing::info!("no RTP from the callee; media timeout");
                return;
            }
        }
    };
    let mut decoders = std::collections::HashMap::new();
    loop {
        let frame = match tokio::time::timeout(timeout, track.recv_audio()).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "the RTP track ended");
                return;
            }
            Err(_) => {
                tracing::info!("no RTP from the callee; media timeout");
                return;
            }
        };
        // Telephone events (DTMF) and unknown payload types carry no audio.
        let Some(codec) = frame.payload_type.and_then(Codec::from_payload_type) else {
            continue;
        };
        let decoder = decoders
            .entry(codec)
            .or_insert_with(|| create_decoder(codec.codec_type()));
        let pcm = decoder.decode(&frame.data);
        if out.send(pcm).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_g711_is_a_codec() {
        assert_eq!(Codec::from_name("pcmu"), Some(Codec::Pcmu));
        assert_eq!(Codec::from_name("PCMA"), Some(Codec::Pcma));
        assert_eq!(Codec::from_name("telephone-event"), None);
        assert_eq!(Codec::from_payload_type(8), Some(Codec::Pcma));
        assert_eq!(Codec::from_payload_type(101), None);
    }
}
