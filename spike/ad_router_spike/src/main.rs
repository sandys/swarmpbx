//! DECISIVE SPIKE: prove a Rust control plane can drive drachtio (SIP) +
//! rtpengine (media) together to originate a call and set up a media-relayed
//! dialog — the core of the "Rust control plane over reused engines" thesis.
//!
//! Flow:
//!   1. rtpengine OFFER (caller SDP)      -> relay SDP A  [we control media]
//!   2. drachtio: send INVITE to the UAS carrying relay SDP A
//!   3. observe UAS 200 OK -> extract answer SDP B
//!   4. rtpengine ANSWER (SDP B)          -> relay SDP for the callee leg
//!   5. teardown: rtpengine DELETE (+ dialog ends)
//!
//! PASS = steps 1–4 all succeed with rtpengine owning both sides' media and
//! drachtio emitting a real INVITE that the UAS answered — i.e. our Rust code
//! orchestrated a real SIP+media call setup end to end.

use std::time::Duration;
use anyhow::{anyhow, Result};
use ad_rtpengine_ctl::RtpengineClient;
use ad_sip_ctl::DrachtioClient;

const CTRL_ADDR: &str = "127.0.0.1:9022";
const SECRET: &str = "spikesecret";
const NG_ADDR: &str = "127.0.0.1:22222";
const UAS: &str = "sip:uas@127.0.0.1:5080";
const FROM_CONTACT: &str = "sip:spike@127.0.0.1";

const CALLER_SDP: &str = "v=0\r\n\
o=rust 1 1 IN IP4 127.0.0.1\r\n\
s=spike\r\n\
c=IN IP4 127.0.0.1\r\n\
t=0 0\r\n\
m=audio 40000 RTP/AVP 0\r\n\
a=rtpmap:0 PCMU/8000\r\n";

fn sdp_from_body(body: &str) -> Option<&str> {
    body.find("\r\n\r\n").map(|p| &body[p + 4..]).filter(|s| s.contains("v=0"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let call_id = "router-spike-1";
    let from_tag = "rust-from-1";

    // --- connect both control planes ---
    let mut rtp = RtpengineClient::connect(NG_ADDR).await?;
    let drachtio = DrachtioClient::connect(CTRL_ADDR).await?;
    drachtio.authenticate(SECRET, "spike").await?;
    println!("PASS: connected + authenticated to drachtio; rtpengine ng ready");

    // --- step 1: rtpengine OFFER (we take control of caller-side media) ---
    let relay_sdp = rtp.offer(call_id, from_tag, CALLER_SDP).await?;
    let relay_line = relay_sdp.lines().find(|l| l.starts_with("m=audio")).unwrap_or("");
    println!("PASS: rtpengine offer -> relay media {relay_line:?}");

    // --- step 2: originate INVITE via drachtio, carrying the relay SDP ---
    let invite = format!(
        "INVITE {UAS} SIP/2.0\r\n\
From: <{FROM_CONTACT}>;tag={from_tag}\r\n\
To: <{UAS}>\r\n\
Contact: <{FROM_CONTACT}>\r\n\
Call-ID: {call_id}\r\n\
CSeq: 1 INVITE\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\
\r\n\
{relay_sdp}",
        relay_sdp.as_bytes().len()
    );

    let mut frames = drachtio.subscribe().await;
    let ack = drachtio.send_sip(&invite).await?;
    println!("PASS: drachtio accepted INVITE for origination (ack {:?})", ack.tokens.get(3));

    // --- step 3: observe network responses; wait for a 2xx with SDP ---
    let mut answer_sdp: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(8), frames.recv()).await {
            Ok(Some(f)) => {
                if f.msg_type == "sip" {
                    let status = f.body.lines().next().unwrap_or("");
                    println!("    <- SIP from network: {status}");
                    if status.contains(" 200 ") {
                        if let Some(sdp) = sdp_from_body(&f.body) {
                            answer_sdp = Some(sdp.to_string());
                            break;
                        }
                    }
                }
            }
            _ => break,
        }
    }

    let answer_sdp = answer_sdp.ok_or_else(|| anyhow!("no 200 OK with SDP from UAS"))?;
    let ans_line = answer_sdp.lines().find(|l| l.starts_with("m=audio")).unwrap_or("");
    println!("PASS: UAS answered with SDP (callee media {ans_line:?})");

    // --- step 4: rtpengine ANSWER (take control of callee-side media too) ---
    let callee_relay = rtp.answer(call_id, from_tag, "uas-tag-1", &answer_sdp).await?;
    let callee_line = callee_relay.lines().find(|l| l.starts_with("m=audio")).unwrap_or("");
    println!("PASS: rtpengine answer -> both legs relayed (callee relay {callee_line:?})");

    // --- step 5: teardown ---
    let del = rtp.delete(call_id, from_tag).await?;
    println!("PASS: rtpengine delete -> {del}");

    println!("\nROUTER SPIKE: PASS — Rust orchestrated rtpengine media + drachtio SIP origination end to end");
    Ok(())
}
