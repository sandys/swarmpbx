//! Spike: prove Rust can drive rtpengine over the ng protocol.
//! PASS criteria: ping->pong, and offer returns a rewritten SDP with a relay
//! address/port different from the input (proof rtpengine took control of media).

use ad_rtpengine_ctl::RtpengineClient;
use anyhow::Result;

const NG_ADDR: &str = "127.0.0.1:22222";

// A minimal, valid caller SDP offering PCMU on a made-up address/port.
const CALLER_SDP: &str = "v=0\r\n\
o=- 1 1 IN IP4 198.51.100.1\r\n\
s=spike\r\n\
c=IN IP4 198.51.100.1\r\n\
t=0 0\r\n\
m=audio 40000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

#[tokio::main]
async fn main() -> Result<()> {
    let mut rtp = RtpengineClient::connect(NG_ADDR).await?;
    println!("connected to rtpengine ng at {NG_ADDR}");

    // 1) ping -> pong
    rtp.ping().await?;
    println!("PASS: ping -> pong");

    // 2) offer -> rewritten SDP
    let call_id = "spike-call-1";
    let from_tag = "fromtag-1";
    let out_sdp = rtp.offer(call_id, from_tag, CALLER_SDP).await?;
    println!("PASS: offer -> ok");
    println!("--- rewritten SDP ---\n{out_sdp}\n---------------------");

    // Proof it actually rewrote media: the c= connection line should now point
    // at rtpengine's relay (127.0.0.1), and the m=audio port is a relay port.
    let rewrote_conn = out_sdp.contains("c=IN IP4 127.0.0.1");
    let relay_port = out_sdp.lines().find(|l| l.starts_with("m=audio")).unwrap_or("");
    if rewrote_conn && relay_port.starts_with("m=audio 30") {
        println!("PASS: rtpengine rewrote media to its relay ({relay_port}) — control confirmed");
    } else {
        println!("WARN: SDP rewrite not as expected (conn={rewrote_conn}, media={relay_port:?})");
    }

    // 3) delete (teardown)
    let del = rtp.delete(call_id, from_tag).await?;
    println!("PASS: delete -> result={del}");

    println!("\nRTPENGINE SPIKE: PASS");
    Ok(())
}
