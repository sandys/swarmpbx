//! Minimal drachtio control-protocol client (spike).
//!
//! Wire format (verified from drachtio-srf lib/wire-protocol.js + drachtio-agent.js):
//!   frame on the wire = `<byteLength>#<body>`
//!   body (outgoing request) = `<msgId>|<message>`
//!   body (incoming) leader (first line up to CRLF) = `<msgId>|<type>|<tokens...>`
//!     type = "response" | "sip" | "cdr" | ...
//! Handshake: send `authenticate|<secret>|<tags>`; response tokens = OK|hostport|version|...
//!
//! This is the piece nobody has written in Rust before — the whole point of the
//! spike is to prove it is small and works against a real drachtio server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex};

/// A parsed inbound frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub msg_id: String,
    pub msg_type: String,     // token[1]: "response", "sip", "cdr", ...
    pub tokens: Vec<String>,  // all leader tokens (incl. msg_id at [0])
    pub body: String,         // anything after the leader's CRLF (raw SIP, etc.)
}

pub struct DrachtioClient {
    writer: Arc<Mutex<OwnedWriteHalf>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Frame>>>>,
    seq: std::sync::atomic::AtomicU64,
    /// Spike aid: every inbound frame is also forwarded here so a caller can
    /// observe the multi-frame SIP transaction (ack frame, 100, 200, ...).
    tap: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Frame>>>>,
}

impl DrachtioClient {
    /// Connect and spawn the read loop. Does not authenticate yet.
    pub async fn connect(addr: &str) -> Result<Arc<Self>> {
        let stream = TcpStream::connect(addr).await.with_context(|| format!("connect {addr}"))?;
        let (mut rd, wr) = stream.into_split();

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Frame>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let tap: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Frame>>>> =
            Arc::new(Mutex::new(None));
        let client = Arc::new(Self {
            writer: Arc::new(Mutex::new(wr)),
            pending: pending.clone(),
            seq: std::sync::atomic::AtomicU64::new(0),
            tap: tap.clone(),
        });

        // Read loop: parse `<len>#<body>` frames, dispatch responses to waiters.
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                match rd.read(&mut tmp).await {
                    Ok(0) => break, // closed
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        while let Some(frame) = try_extract_frame(&mut buf) {
                            // Forward every frame to the tap (if any) for observation.
                            if let Some(tx) = tap.lock().await.as_ref() {
                                let _ = tx.send(frame.clone());
                            }
                            // Correlation key: for a "response" frame the leader is
                            //   <serverMsgId>|response|<originalMsgId>|OK|...
                            // so the id WE sent is token[2]. For other frames the
                            // id is token[0].
                            let key = if frame.msg_type == "response" {
                                frame.tokens.get(2).cloned().unwrap_or_default()
                            } else {
                                frame.msg_id.clone()
                            };
                            if let Some(tx) = pending.lock().await.remove(&key) {
                                let _ = tx.send(frame);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[drachtio] read error: {e}");
                        break;
                    }
                }
            }
        });

        Ok(client)
    }

    /// Subscribe to all inbound frames (spike aid for multi-frame transactions).
    pub async fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<Frame> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *self.tap.lock().await = Some(tx);
        rx
    }

    /// Send a request body (`msgId` is prepended automatically) and await its response.
    pub async fn request(&self, message: &str) -> Result<Frame> {
        let id = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let msg_id = format!("spike-{id}");
        let body = format!("{msg_id}|{message}");
        let framed = format!("{}#{}", body.as_bytes().len(), body);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(msg_id.clone(), tx);

        self.writer.lock().await.write_all(framed.as_bytes()).await?;

        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .context("drachtio response timed out")?
            .map_err(|_| anyhow!("response channel dropped"))
    }

    /// authenticate|secret|tags  ->  response frame: ...|OK|hostport|version|...
    pub async fn authenticate(&self, secret: &str, tags: &str) -> Result<Frame> {
        let resp = self.request(&format!("authenticate|{secret}|{tags}")).await?;
        // leader tokens: [serverMsgId, "response", originalMsgId, "OK", hostport, version, ...]
        let ok = resp.tokens.get(3).map(|s| s == "OK").unwrap_or(false);
        if !ok {
            return Err(anyhow!("authenticate failed: {:?}", resp.tokens));
        }
        Ok(resp)
    }

    /// Send a raw outbound SIP request (UAC) and await the first response frame.
    /// Wire: `sip|<stackTxnId>|<stackDialogId>\r\n<raw SIP>`. Empty ids = new request.
    /// drachtio first replies with an ack frame (OK + assigned stackTxnId); then
    /// network responses arrive as further frames correlated to the same msgId.
    pub async fn send_sip(&self, raw_sip: &str) -> Result<Frame> {
        let message = format!("sip|||\r\n{raw_sip}");
        self.request(&message).await
    }
}

/// Try to pull one complete `<len>#<body>` frame from the buffer.
fn try_extract_frame(buf: &mut Vec<u8>) -> Option<Frame> {
    let hash_pos = buf.iter().position(|&c| c == b'#')?;
    let len_str = std::str::from_utf8(&buf[..hash_pos]).ok()?;
    let msg_size: usize = len_str.trim().parse().ok()?;
    let start = hash_pos + 1;
    let total = start + msg_size;
    if buf.len() < total {
        return None; // need more bytes
    }
    let body = String::from_utf8_lossy(&buf[start..total]).into_owned();
    buf.drain(..total);

    // Leader = up to first CRLF (or whole body); tokens split on '|'.
    let (leader, rest) = match body.find("\r\n") {
        Some(p) => (&body[..p], &body[p + 2..]),
        None => (body.as_str(), ""),
    };
    let tokens: Vec<String> = leader.split('|').map(|s| s.to_string()).collect();
    Some(Frame {
        msg_id: tokens.first().cloned().unwrap_or_default(),
        msg_type: tokens.get(1).cloned().unwrap_or_default(),
        tokens,
        body: rest.to_string(),
    })
}
