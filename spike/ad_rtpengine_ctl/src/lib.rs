//! Minimal rtpengine `ng` control client (spike).
//!
//! Wire format (verified against rtpengine mr9.4): each UDP datagram is
//!   `<cookie> <bencode-dict>`
//! The cookie is echoed in the reply. The dict is **bencode** on the UDP ng
//! transport for this build (JSON was rejected with "Could not decode
//! dictionary" — a plan assumption we corrected during the spike).

pub mod bencode;

use std::time::Duration;
use anyhow::{anyhow, Context, Result};
use bencode::{dict, Ben};
use tokio::net::UdpSocket;

pub struct RtpengineClient {
    sock: UdpSocket,
    seq: u64,
}

impl RtpengineClient {
    pub async fn connect(addr: &str) -> Result<Self> {
        let sock = UdpSocket::bind("127.0.0.1:0").await?;
        sock.connect(addr).await.with_context(|| format!("connect {addr}"))?;
        Ok(Self { sock, seq: 0 })
    }

    pub async fn command(&mut self, body: Ben) -> Result<Ben> {
        self.seq += 1;
        let cookie = format!("{}_{}", std::process::id(), self.seq);

        let mut datagram = Vec::new();
        datagram.extend_from_slice(cookie.as_bytes());
        datagram.push(b' ');
        datagram.extend_from_slice(&body.encode());
        self.sock.send(&datagram).await?;

        let mut buf = vec![0u8; 64 * 1024];
        let n = tokio::time::timeout(Duration::from_secs(3), self.sock.recv(&mut buf))
            .await
            .context("rtpengine recv timed out")??;

        // Reply = `<cookie> <bencode>`; split at first space.
        let sp = buf[..n]
            .iter()
            .position(|&c| c == b' ')
            .ok_or_else(|| anyhow!("malformed reply (no space)"))?;
        let reply_cookie = std::str::from_utf8(&buf[..sp])?;
        if reply_cookie != cookie {
            return Err(anyhow!("cookie mismatch: sent {cookie}, got {reply_cookie}"));
        }
        Ben::decode(&buf[sp + 1..n]).map_err(|e| anyhow!("decode reply: {e}"))
    }

    pub async fn ping(&mut self) -> Result<()> {
        let resp = self.command(dict(vec![("command", Ben::Str("ping".into()))])).await?;
        match resp.get("result").and_then(|v| v.as_str()) {
            Some("pong") => Ok(()),
            other => Err(anyhow!("unexpected ping result: {other:?} (full: {resp:?})")),
        }
    }

    /// `offer`: hand rtpengine the caller SDP; it allocates relay ports and
    /// returns rewritten SDP. Returns the rewritten SDP string.
    pub async fn offer(&mut self, call_id: &str, from_tag: &str, sdp: &str) -> Result<String> {
        let resp = self
            .command(dict(vec![
                ("command", Ben::Str("offer".into())),
                ("call-id", Ben::Str(call_id.into())),
                ("from-tag", Ben::Str(from_tag.into())),
                ("sdp", Ben::Str(sdp.into())),
            ]))
            .await?;
        if resp.get("result").and_then(|v| v.as_str()) != Some("ok") {
            return Err(anyhow!("offer failed: {resp:?}"));
        }
        Ok(resp.get("sdp").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    /// `answer`: hand rtpengine the callee SDP for the same call.
    pub async fn answer(&mut self, call_id: &str, from_tag: &str, to_tag: &str, sdp: &str) -> Result<String> {
        let resp = self
            .command(dict(vec![
                ("command", Ben::Str("answer".into())),
                ("call-id", Ben::Str(call_id.into())),
                ("from-tag", Ben::Str(from_tag.into())),
                ("to-tag", Ben::Str(to_tag.into())),
                ("sdp", Ben::Str(sdp.into())),
            ]))
            .await?;
        if resp.get("result").and_then(|v| v.as_str()) != Some("ok") {
            return Err(anyhow!("answer failed: {resp:?}"));
        }
        Ok(resp.get("sdp").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    pub async fn delete(&mut self, call_id: &str, from_tag: &str) -> Result<String> {
        let resp = self
            .command(dict(vec![
                ("command", Ben::Str("delete".into())),
                ("call-id", Ben::Str(call_id.into())),
                ("from-tag", Ben::Str(from_tag.into())),
            ]))
            .await?;
        Ok(resp.get("result").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }
}
