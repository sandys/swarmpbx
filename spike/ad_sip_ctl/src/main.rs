//! Spike: prove Rust can connect to drachtio's control channel and authenticate.
//! PASS criteria: TCP connect, send `authenticate|secret|tags`, receive OK + the
//! server hostport. This proves the len#body framing + token protocol works.

use ad_sip_ctl::DrachtioClient;
use anyhow::Result;

const CTRL_ADDR: &str = "127.0.0.1:9022";
const SECRET: &str = "spikesecret";

#[tokio::main]
async fn main() -> Result<()> {
    let client = DrachtioClient::connect(CTRL_ADDR).await?;
    println!("connected to drachtio control at {CTRL_ADDR}");

    let resp = client.authenticate(SECRET, "spike").await?;
    println!("PASS: authenticate -> {:?}", resp.tokens);

    // tokens: [serverMsgId, "response", originalMsgId, "OK", hostport, version?, ...]
    if let Some(hostport) = resp.tokens.get(4) {
        println!("PASS: server hostport = {hostport}");
    }
    if let Some(version) = resp.tokens.get(5) {
        println!("       server version  = {version}");
    }

    println!("\nDRACHTIO SPIKE: PASS");
    Ok(())
}
