//! Two real RTP legs on loopback: an offerer (the dialer) and an answerer
//! (the callee). Audio must cross in both directions as G.711.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use ad_media::{Codec, MediaConfig, RtpLeg, FRAME_SAMPLES};

fn loopback() -> MediaConfig {
    MediaConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// A 400 Hz tone, 20 ms at 8 kHz.
fn tone() -> Vec<i16> {
    (0..FRAME_SAMPLES)
        .map(|i| {
            let t = i as f32 / 8000.0;
            (8000.0 * (2.0 * std::f32::consts::PI * 400.0 * t).sin()) as i16
        })
        .collect()
}

fn loud(frame: &[i16]) -> bool {
    frame
        .iter()
        .map(|s| s.unsigned_abs() as u32)
        .max()
        .unwrap_or(0)
        > 1000
}

/// Wait for a frame with audio in it (the send clock sends silence first).
async fn heard(rx: &mut tokio::sync::mpsc::Receiver<Vec<i16>>) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        if loud(&frame) {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn audio_crosses_both_ways_as_g711() {
    let (dialer, offer) = RtpLeg::offer(&loopback()).await.unwrap();
    assert!(offer.contains("m=audio"), "{offer}");
    assert!(
        offer.contains("PCMU/8000") && offer.contains("PCMA/8000"),
        "{offer}"
    );
    assert!(offer.contains("telephone-event"), "{offer}");

    let (callee, answer) = RtpLeg::answer(&loopback(), &offer).await.unwrap();
    dialer.accept(&answer).await.unwrap();
    assert_eq!(dialer.codec().unwrap(), Codec::Pcmu);

    let mut dialer = dialer.start().unwrap();
    let mut callee = callee.start().unwrap();
    let mut at_callee = callee.take_inbound().unwrap();
    let mut at_dialer = dialer.take_inbound().unwrap();

    let feed = |m: ad_media::MediaOut| {
        tokio::spawn(async move {
            for _ in 0..100 {
                m.send(tone());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let a = feed(dialer.sender());
    let b = feed(callee.sender());
    assert!(heard(&mut at_callee).await, "the callee heard the dialer");
    assert!(heard(&mut at_dialer).await, "the dialer heard the callee");
    a.abort();
    b.abort();
}

#[tokio::test]
async fn the_answer_decides_the_codec() {
    let (dialer, _offer) = RtpLeg::offer(&loopback()).await.unwrap();
    // A carrier that supports only PCMA answers with PCMA.
    let answer = "v=0\r\n\
        o=carrier 1 1 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        c=IN IP4 127.0.0.1\r\n\
        t=0 0\r\n\
        m=audio 40000 RTP/AVP 8\r\n\
        a=rtpmap:8 PCMA/8000\r\n\
        a=sendrecv\r\n";
    dialer.accept(answer).await.unwrap();
    assert_eq!(dialer.codec().unwrap(), Codec::Pcma);
}

#[tokio::test]
async fn no_rtp_ends_the_inbound_audio_at_the_timeout() {
    let mut cfg = loopback();
    cfg.timeout = Duration::from_millis(500);
    let (dialer, offer) = RtpLeg::offer(&cfg).await.unwrap();
    // A callee leg that never starts: no RTP comes back.
    let (_silent, answer) = RtpLeg::answer(&loopback(), &offer).await.unwrap();
    dialer.accept(&answer).await.unwrap();
    let mut dialer = dialer.start().unwrap();
    let mut inbound = dialer.take_inbound().unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while inbound.recv().await.is_some() {}
    })
    .await;
    assert!(
        ended.is_ok(),
        "the inbound channel closed after the timeout"
    );
}

/// A carrier that latches: it sends nothing until RTP arrives, then sends each
/// packet back to the address it came from (like SIPp -rtp_echo). Our leg
/// must send first, and hear its own audio back.
#[tokio::test]
async fn a_latching_echo_carrier_returns_our_audio() {
    let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = echo.local_addr().unwrap().port();
    let carrier = tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = echo.recv_from(&mut buf).await {
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });
    let (dialer, _offer) = RtpLeg::offer(&loopback()).await.unwrap();
    let answer = format!(
        "v=0\r\no=carrier 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
         t=0 0\r\nm=audio {port} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n"
    );
    dialer.accept(&answer).await.unwrap();
    let mut dialer = dialer.start().unwrap();
    let mut back = dialer.take_inbound().unwrap();
    let out = dialer.sender();
    let feed = tokio::spawn(async move {
        for _ in 0..150 {
            out.send(tone());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    assert!(heard(&mut back).await, "our tone came back from the echo");
    feed.abort();
    carrier.abort();
}

/// With nothing to send (a quiet agent), the leg still sends a packet every
/// 20 ms, so the NAT mapping stays open and a latching carrier keeps sending.
#[tokio::test]
async fn a_quiet_leg_still_sends_so_the_echo_keeps_coming() {
    let echo = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = echo.local_addr().unwrap().port();
    let carrier = tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = echo.recv_from(&mut buf).await {
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });
    let (dialer, _offer) = RtpLeg::offer(&loopback()).await.unwrap();
    let answer = format!(
        "v=0\r\no=carrier 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
         t=0 0\r\nm=audio {port} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n"
    );
    dialer.accept(&answer).await.unwrap();
    let mut dialer = dialer.start().unwrap();
    let mut back = dialer.take_inbound().unwrap();
    // One second of a quiet agent: about 50 packets of silence come back.
    let mut frames = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, back.recv()).await {
        assert!(!loud(&frame), "silence goes out when nothing is queued");
        frames += 1;
    }
    assert!(frames >= 40, "only {frames} packets in one second");
    carrier.abort();
}
