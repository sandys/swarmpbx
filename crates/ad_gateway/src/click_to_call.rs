//! Click-to-call through Twilio Programmable Voice (GAPS.md CARRIER-2).
//!
//! Twilio places the call to the phone. The node asks Twilio's Calls API to
//! call the lead, with inline TwiML for the answer:
//! `<Dial><Sip>{sip_uri}?x-ad-call={token}&x-ad-node={node}</Sip></Dial>`.
//! When the lead answers, Twilio sends an INVITE to the SIP edge. The edge
//! routes it to this node by `x-ad-node`, and the node answers the INVITE that
//! carries its token (`ad_sip::SipUa::expect`) and connects it to the room.
//! Until the INVITE arrives, the node reads the call's status from the Calls
//! API: busy, no answer, and failed end the dial. No webhook is needed, so
//! the node needs no public HTTP address.
//!
//! The node cancels the Twilio call when the dial ends without the INVITE
//! (also when the dial task stops). After the INVITE, the SIP dialog owns the
//! call: our BYE ends Twilio's `<Dial>`, and Twilio then ends the phone call.

use std::fmt;
use std::time::Duration;

use ad_dispatch::Placement;
use serde::Deserialize;

use crate::sip_call::{Dialed, SipCaller, SipDial};
use crate::RING_TIMEOUT;

/// How often the node reads the call's status while it waits.
const POLL: Duration = Duration::from_secs(1);
/// After the lead answers, the longest wait for Twilio's INVITE.
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest wait for the lead to ring, after the ring timeout.
const QUEUE_GRACE: Duration = Duration::from_secs(15);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// The Twilio account and how Twilio reaches the SIP edge. `Debug` omits the
/// secret.
#[derive(Clone)]
pub struct TwilioVoice {
    /// `https://api.twilio.com`, or the simulator.
    pub api_url: String,
    pub account_sid: String,
    /// An API key SID (`SK...`), or the account SID with the auth token.
    pub key_sid: String,
    pub key_secret: String,
    /// The SIP URI that Twilio dials to reach the SIP edge, for example
    /// `sip:agentdialer@edge.example.com:5060;transport=tcp`.
    pub sip_uri: String,
    /// The From of the call to the lead: a Twilio number of the account or a
    /// verified caller ID.
    pub caller_id: String,
}

impl fmt::Debug for TwilioVoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwilioVoice")
            .field("api_url", &self.api_url)
            .field("account_sid", &self.account_sid)
            .field("key_sid", &self.key_sid)
            .field("sip_uri", &self.sip_uri)
            .field("caller_id", &self.caller_id)
            .finish_non_exhaustive()
    }
}

/// Places calls through Twilio, and carries them with our SIP user agent and
/// RTP legs once Twilio calls us.
#[derive(Clone)]
pub struct ClickToCall {
    sip: SipCaller,
    twilio: TwilioVoice,
    http: reqwest::Client,
}

/// A Twilio call resource, as much as the node reads.
#[derive(Debug, Deserialize)]
struct TwilioCall {
    sid: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct TwilioFault {
    code: Option<u32>,
    message: Option<String>,
}

/// Twilio's reason for a refused request. Twilio sends code 0 for some
/// refusals (a Trial account's disallowed parameters), so the HTTP status
/// stands in for it.
fn refusal(status: reqwest::StatusCode, fault: Option<TwilioFault>) -> String {
    let (code, message) = fault.map_or((None, None), |f| (f.code, f.message));
    match (code.filter(|&c| c != 0), message) {
        (Some(code), Some(message)) => format!("Twilio {code}: {message}"),
        (None, Some(message)) => format!("Twilio: HTTP {status}: {message}"),
        (_, None) => format!("Twilio: HTTP {status}"),
    }
}

/// What the call's status means for the dial.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Progress {
    /// Queued or ringing.
    Ringing,
    /// The lead answered; Twilio's INVITE comes next.
    LeadAnswered,
    /// The call ended before it reached the node.
    Ended(Placement),
}

/// Twilio's call status (Call resource, `status`) for a call whose INVITE
/// has not reached the node. Pure.
fn progress(status: &str) -> Progress {
    let rejected = |code: u16, detail: &str| {
        Progress::Ended(Placement::Rejected {
            sip_code: Some(code),
            detail: format!("Twilio: {detail}"),
        })
    };
    match status {
        "queued" | "initiated" | "ringing" => Progress::Ringing,
        "in-progress" => Progress::LeadAnswered,
        "busy" => rejected(486, "busy"),
        "no-answer" => rejected(480, "no answer"),
        "canceled" => rejected(487, "canceled"),
        "completed" => Progress::Ended(Placement::Rejected {
            sip_code: None,
            detail: "Twilio: the call ended before its INVITE reached the node".into(),
        }),
        other => Progress::Ended(Placement::Rejected {
            sip_code: None,
            detail: format!("Twilio: {other}"),
        }),
    }
}

/// The TwiML that Twilio runs when the lead answers. Pure.
fn twiml(sip_uri: &str, token: &str, node: &str) -> String {
    let sep = if sip_uri.contains('?') { '&' } else { '?' };
    let uri = format!("{sip_uri}{sep}x-ad-call={token}&x-ad-node={node}");
    format!(
        "<Response><Dial><Sip>{}</Sip></Dial></Response>",
        xml_text(&uri)
    )
}

fn xml_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 128 random bits as hex: the call's token.
fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Ends the Twilio call when it drops armed: the dial ended without the
/// INVITE.
struct Pending<'a> {
    owner: &'a ClickToCall,
    sid: String,
    armed: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if self.armed {
            let owner = self.owner.clone();
            let sid = std::mem::take(&mut self.sid);
            tokio::spawn(async move { owner.end(&sid).await });
        }
    }
}

impl ClickToCall {
    pub fn new(sip: SipCaller, twilio: TwilioVoice) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { sip, twilio, http }
    }

    fn calls_url(&self) -> String {
        format!(
            "{}/2010-04-01/Accounts/{}/Calls",
            self.twilio.api_url.trim_end_matches('/'),
            self.twilio.account_sid
        )
    }

    /// Ask Twilio to call the lead, then connect the call when Twilio's
    /// INVITE arrives.
    pub async fn dial(&self, req: SipDial<'_>) -> Dialed {
        let token = new_token();
        let mut expected = self.sip.ua().expect(&token);
        let node = self.sip.ua().contact_addr().to_string();
        let ring_s = RING_TIMEOUT.as_secs().to_string();
        let form = [
            ("To", req.phone_number),
            ("From", self.twilio.caller_id.as_str()),
            ("Twiml", &twiml(&self.twilio.sip_uri, &token, &node)),
            ("Timeout", ring_s.as_str()),
        ];
        let created = match self.create(&form).await {
            Ok(call) => call,
            Err(placement) => return Dialed::Failed(placement),
        };
        tracing::info!(
            call_sid = created.sid,
            room = req.room,
            "click-to-call: Twilio is calling the lead"
        );
        let mut pending = Pending {
            owner: self,
            sid: created.sid,
            armed: true,
        };

        let mut answered_at = None;
        let deadline = tokio::time::Instant::now() + RING_TIMEOUT + BRIDGE_TIMEOUT + QUEUE_GRACE;
        let mut tick = tokio::time::interval(POLL);
        loop {
            tokio::select! {
                incoming = expected.arrived() => {
                    let Some(incoming) = incoming else {
                        return Dialed::Failed(Placement::NotSent("the SIP user agent stopped".into()));
                    };
                    tracing::info!(
                        call_sid = pending.sid,
                        sip_call_id = incoming.call_id,
                        "click-to-call: Twilio's INVITE arrived"
                    );
                    pending.armed = false;
                    return self.sip.answer(incoming, req.room, req.identity, req.on_answer).await;
                }
                _ = tick.tick() => {}
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Dialed::Failed(Placement::Rejected {
                    sip_code: Some(408),
                    detail: "Twilio did not reach the lead in time".into(),
                });
            }
            if answered_at.is_some_and(|at| now >= at + BRIDGE_TIMEOUT) {
                return Dialed::Failed(Placement::Rejected {
                    sip_code: None,
                    detail: "the lead answered, but Twilio's INVITE did not reach the node".into(),
                });
            }
            match self.status(&pending.sid).await {
                Ok(status) => match progress(&status) {
                    Progress::Ringing => {}
                    Progress::LeadAnswered => {
                        answered_at.get_or_insert(now);
                    }
                    Progress::Ended(placement) => {
                        pending.armed = false;
                        return Dialed::Failed(placement);
                    }
                },
                Err(e) => tracing::debug!(call_sid = pending.sid, error = e, "status read failed"),
            }
        }
    }

    /// Create the call. A refusal means that no call exists; a lost answer
    /// means that one may exist.
    async fn create(&self, form: &[(&str, &str)]) -> Result<TwilioCall, Placement> {
        let sent = self
            .http
            .post(format!("{}.json", self.calls_url()))
            .basic_auth(&self.twilio.key_sid, Some(&self.twilio.key_secret))
            .form(form)
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) if e.is_connect() => {
                return Err(Placement::NotSent(format!("Twilio: {e}")));
            }
            Err(e) => return Err(Placement::NoResponse(format!("Twilio: {e}"))),
        };
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<TwilioCall>()
                .await
                .map_err(|e| Placement::NoResponse(format!("Twilio: bad call resource: {e}")));
        }
        let detail = refusal(status, resp.json::<TwilioFault>().await.ok());
        if status.is_client_error() {
            Err(Placement::NotSent(detail))
        } else {
            Err(Placement::NoResponse(detail))
        }
    }

    async fn status(&self, sid: &str) -> Result<String, String> {
        let resp = self
            .http
            .get(format!("{}/{sid}.json", self.calls_url()))
            .basic_auth(&self.twilio.key_sid, Some(&self.twilio.key_secret))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        resp.json::<TwilioCall>()
            .await
            .map(|c| c.status)
            .map_err(|e| e.to_string())
    }

    /// End the call: cancel it while it rings, hang it up after the answer.
    async fn end(&self, sid: &str) {
        for status in ["canceled", "completed"] {
            let done = self
                .http
                .post(format!("{}/{sid}.json", self.calls_url()))
                .basic_auth(&self.twilio.key_sid, Some(&self.twilio.key_secret))
                .form(&[("Status", status)])
                .send()
                .await
                .and_then(|r| r.error_for_status());
            match done {
                Ok(_) => {
                    tracing::info!(
                        call_sid = sid,
                        status,
                        "click-to-call: ended the Twilio call"
                    );
                    return;
                }
                Err(e) => tracing::debug!(call_sid = sid, status, error = %e, "end failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_twiml_dials_the_edge_with_the_token_and_the_node() {
        assert_eq!(
            twiml(
                "sip:agentdialer@edge.example:5060;transport=tcp",
                "ab12",
                "10.20.0.21:5060"
            ),
            "<Response><Dial><Sip>sip:agentdialer@edge.example:5060;transport=tcp\
             ?x-ad-call=ab12&amp;x-ad-node=10.20.0.21:5060</Sip></Dial></Response>"
        );
        assert!(twiml("sip:a@b?x-tenant=1", "t", "n").contains("?x-tenant=1&amp;x-ad-call=t"));
    }

    #[test]
    fn a_refusal_names_twilios_code_or_else_the_http_status() {
        let bad = reqwest::StatusCode::BAD_REQUEST;
        let fault = |code, message: &str| {
            Some(TwilioFault {
                code,
                message: Some(message.into()),
            })
        };
        assert_eq!(
            refusal(bad, fault(Some(21219), "not verified")),
            "Twilio 21219: not verified"
        );
        assert_eq!(
            refusal(
                bad,
                fault(Some(0), "trial accounts have limited parameter access")
            ),
            "Twilio: HTTP 400 Bad Request: trial accounts have limited parameter access"
        );
        assert_eq!(refusal(bad, None), "Twilio: HTTP 400 Bad Request");
    }

    #[test]
    fn twilios_status_decides_the_dial() {
        assert_eq!(progress("queued"), Progress::Ringing);
        assert_eq!(progress("ringing"), Progress::Ringing);
        assert_eq!(progress("in-progress"), Progress::LeadAnswered);
        let code = |s: &str| match progress(s) {
            Progress::Ended(Placement::Rejected { sip_code, .. }) => sip_code,
            other => panic!("{s}: {other:?}"),
        };
        // The codes map to the outcome classes: busy, no answer, no answer.
        assert_eq!(code("busy"), Some(486));
        assert_eq!(code("no-answer"), Some(480));
        assert_eq!(code("canceled"), Some(487));
        assert_eq!(code("failed"), None);
        assert_eq!(code("completed"), None);
    }

    #[test]
    fn a_token_is_128_random_bits() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
