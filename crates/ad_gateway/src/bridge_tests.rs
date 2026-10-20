use super::*;
use std::sync::{Arc, Mutex};

/// A fake room session: records every outbound frame, and delivers a scripted
/// inbound stream (the "agent" track).
struct FakeSession {
    published: Arc<Mutex<Vec<AudioFrame>>>,
    inbound: Option<mpsc::Receiver<AudioFrame>>,
    closed: Arc<Mutex<bool>>,
}

impl RoomSession for FakeSession {
    async fn send_audio(&self, frame: AudioFrame) -> Result<(), BridgeError> {
        self.published.lock().unwrap().push(frame);
        Ok(())
    }
    fn take_inbound(&mut self) -> Option<mpsc::Receiver<AudioFrame>> {
        self.inbound.take()
    }
    async fn close(self) {
        *self.closed.lock().unwrap() = true;
    }
}

/// A session whose outbound publish fails. Its inbound channel stays open with
/// no frames, so the bridge can only end through the outbound pump's exit.
struct FailingSession {
    inbound: Option<mpsc::Receiver<AudioFrame>>,
}

impl RoomSession for FailingSession {
    async fn send_audio(&self, _frame: AudioFrame) -> Result<(), BridgeError> {
        Err(BridgeError::Publish("track gone".into()))
    }
    fn take_inbound(&mut self) -> Option<mpsc::Receiver<AudioFrame>> {
        self.inbound.take()
    }
    async fn close(self) {}
}

/// A channel-backed RTP source.
struct ChanSource(mpsc::Receiver<AudioFrame>);
impl RtpSource for ChanSource {
    async fn recv(&mut self) -> Option<AudioFrame> {
        self.0.recv().await
    }
}

/// A channel-backed RTP sink that records frames sent to the caller.
struct ChanSink(Arc<Mutex<Vec<AudioFrame>>>);
impl RtpSink for ChanSink {
    async fn send(&self, frame: AudioFrame) -> Result<(), BridgeError> {
        self.0.lock().unwrap().push(frame);
        Ok(())
    }
}

fn frame(v: i16) -> AudioFrame {
    AudioFrame::new(vec![v; 160], 8000, 1) // 20ms @ 8kHz mono
}

#[test]
fn audio_frame_computes_samples_per_channel() {
    let f = AudioFrame::new(vec![0; 320], 16000, 2);
    assert_eq!(f.samples_per_channel, 160);
    let s = AudioFrame::silence(8000, 1, 160);
    assert_eq!(s.samples.len(), 160);
    assert!(s.samples.iter().all(|&x| x == 0));
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_pumps_audio_in_both_directions() {
    let published = Arc::new(Mutex::new(Vec::new()));
    let to_caller = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(Mutex::new(false));

    // RTP side: 3 frames from the caller, then end.
    let (rtp_tx, rtp_rx) = mpsc::channel(8);
    for v in [1, 2, 3] {
        rtp_tx.send(frame(v)).await.unwrap();
    }
    drop(rtp_tx); // caller hangs up -> source ends

    // Room side: the agent publishes 2 frames, then the track ends.
    let (room_tx, room_rx) = mpsc::channel(8);
    for v in [10, 20] {
        room_tx.send(frame(v)).await.unwrap();
    }
    drop(room_tx);

    let session = FakeSession {
        published: published.clone(),
        inbound: Some(room_rx),
        closed: closed.clone(),
    };
    let stats = MediaBridge::run(session, ChanSource(rtp_rx), ChanSink(to_caller.clone()))
        .await
        .unwrap();

    // Outbound: the 3 caller frames reached the room's published track.
    assert_eq!(stats.outbound_frames, 3);
    assert_eq!(published.lock().unwrap().len(), 3);
    assert_eq!(published.lock().unwrap()[0], frame(1));

    // Inbound: the 2 agent frames reached the caller via the RTP sink.
    assert_eq!(stats.inbound_frames, 2);
    assert_eq!(to_caller.lock().unwrap().len(), 2);
    assert_eq!(to_caller.lock().unwrap()[0], frame(10));

    // The session was closed when both directions ended.
    assert!(*closed.lock().unwrap());
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_ends_when_only_rtp_has_data_and_room_closes_immediately() {
    let published = Arc::new(Mutex::new(Vec::new()));
    let to_caller = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(Mutex::new(false));

    let (rtp_tx, rtp_rx) = mpsc::channel(8);
    rtp_tx.send(frame(7)).await.unwrap();
    drop(rtp_tx);

    // Agent never publishes: the inbound channel closes with no frames.
    let (room_tx, room_rx) = mpsc::channel::<AudioFrame>(1);
    drop(room_tx);

    let session = FakeSession {
        published: published.clone(),
        inbound: Some(room_rx),
        closed: closed.clone(),
    };
    let stats = MediaBridge::run(session, ChanSource(rtp_rx), ChanSink(to_caller))
        .await
        .unwrap();

    assert_eq!(stats.outbound_frames, 1);
    assert_eq!(stats.inbound_frames, 0, "no agent audio");
    assert!(*closed.lock().unwrap());
}

#[test]
fn bridge_returns_publish_error_and_does_not_hang() {
    // The bridge runs on its own thread and runtime, so a hang shows up here as
    // a clean timeout instead of a stuck test binary.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            // The caller keeps sending: the outbound pump must end on the
            // publish error, not because the RTP source ended.
            let (rtp_tx, rtp_rx) = mpsc::channel(8);
            rtp_tx.send(frame(1)).await.unwrap();
            // The agent channel stays open with no frames: the inbound pump
            // must end because the outbound pump ended, not on agent audio.
            let (_room_tx, room_rx) = mpsc::channel::<AudioFrame>(1);
            let session = FailingSession {
                inbound: Some(room_rx),
            };
            let sink = ChanSink(Arc::new(Mutex::new(Vec::new())));
            MediaBridge::run(session, ChanSource(rtp_rx), sink).await
        });
        let _ = done_tx.send(result);
    });
    let result = done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("bridge must end when the outbound pump fails");
    assert!(matches!(result, Err(BridgeError::Publish(_))));
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_errors_if_inbound_already_taken() {
    let (_tx, rx) = mpsc::channel::<AudioFrame>(1);
    let mut session = FakeSession {
        published: Arc::new(Mutex::new(Vec::new())),
        inbound: Some(rx),
        closed: Arc::new(Mutex::new(false)),
    };
    // Steal the inbound receiver so the bridge cannot take it.
    let _stolen = session.take_inbound();
    let (_rtp_tx, rtp_rx) = mpsc::channel(1);
    let sink = ChanSink(Arc::new(Mutex::new(Vec::new())));
    let err = MediaBridge::run(session, ChanSource(rtp_rx), sink)
        .await
        .unwrap_err();
    assert!(matches!(err, BridgeError::Subscribe(_)));
}
