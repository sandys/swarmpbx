//! `webrtc_bridge_probe` — drives OUR native WebRTC participant end to end
//! (feature `webrtc`). It joins a LiveKit room, publishes a generated tone as
//! the outbound ("SIP-side") track via `MediaBridge`, and counts the agent's
//! inbound audio frames. It prints JSON stats and exits non-zero on failure.
//! There is NO `livekit/sip` here — this is our own media leg.
//!
//! Env:
//!   LIVEKIT_URL, LIVEKIT_API_KEY, LIVEKIT_API_SECRET
//!   AD_ROOM (default "campaign-1"), AD_IDENTITY (default "sip-bridge")
//!   AD_SECONDS (default 6) — how long to pump audio.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ad_gateway::bridge::{
    AudioFrame, BridgeError, MediaBridge, RoomConnection, RoomParticipant, RtpSink, RtpSource,
};
use ad_gateway::{mint_token, LiveKitParticipant};
use tokio::sync::mpsc;

/// An RTP source that generates a 440 Hz tone in 10 ms frames at 48 kHz mono,
/// standing in for the caller audio arriving from rtpengine. Stops after a set
/// duration.
struct ToneSource {
    sample_rate: u32,
    phase: f32,
    frames_left: u32,
}
impl RtpSource for ToneSource {
    async fn recv(&mut self) -> Option<AudioFrame> {
        if self.frames_left == 0 {
            return None;
        }
        self.frames_left -= 1;
        let spc = (self.sample_rate / 100) as usize; // 10 ms
        let step = 2.0 * std::f32::consts::PI * 440.0 / self.sample_rate as f32;
        let mut samples = Vec::with_capacity(spc);
        for _ in 0..spc {
            self.phase += step;
            samples.push((self.phase.sin() * 8000.0) as i16);
        }
        // Pace at ~real time so we do not blast all frames instantly.
        tokio::time::sleep(Duration::from_millis(10)).await;
        Some(AudioFrame::new(samples, self.sample_rate, 1))
    }
}

/// A sink that counts the agent frames we would send back to the caller.
struct CountingSink {
    count: Arc<std::sync::atomic::AtomicU64>,
}
impl RtpSink for CountingSink {
    async fn send(&self, _frame: AudioFrame) -> Result<(), BridgeError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let url = std::env::var("LIVEKIT_URL").expect("LIVEKIT_URL");
    let key = std::env::var("LIVEKIT_API_KEY").expect("LIVEKIT_API_KEY");
    let secret = std::env::var("LIVEKIT_API_SECRET").expect("LIVEKIT_API_SECRET");
    let room = std::env::var("AD_ROOM").unwrap_or_else(|_| "campaign-1".into());
    let identity = std::env::var("AD_IDENTITY").unwrap_or_else(|_| "sip-bridge".into());
    let seconds: u32 = std::env::var("AD_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    let token = mint_token(&key, &secret, &room, &identity).expect("mint token");
    let participant = LiveKitParticipant::default();
    let session = participant
        .join(RoomConnection {
            url,
            token,
            room: room.clone(),
            identity,
        })
        .await
        .expect("join room");

    let inbound_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let source = ToneSource {
        sample_rate: 48000,
        phase: 0.0,
        frames_left: seconds * 100,
    };
    let sink = CountingSink {
        count: inbound_count.clone(),
    };

    // A watchdog that ends the run if the bridge never stops on its own.
    let done = Arc::new(AtomicBool::new(false));
    let done2 = done.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(seconds as u64 + 4)).await;
        done2.store(true, Ordering::SeqCst);
    });

    let stats = MediaBridge::run(session, source, sink)
        .await
        .expect("bridge run");
    let inbound = inbound_count.load(Ordering::SeqCst);

    println!(
        "{{\"outbound_frames\":{},\"inbound_frames\":{}}}",
        stats.outbound_frames, inbound
    );
    // Success = we published audio (outbound) AND received agent audio (inbound).
    if stats.outbound_frames > 0 && stats.inbound_frames > 0 {
        std::process::exit(0);
    } else {
        eprintln!(
            "FAIL: outbound={} inbound={} (need both > 0)",
            stats.outbound_frames, stats.inbound_frames
        );
        std::process::exit(1);
    }
}
