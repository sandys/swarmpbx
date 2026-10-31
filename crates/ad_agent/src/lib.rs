//! Agent runtime adapters (PLAN.md §0.10.7a, decision D-2).
//!
//! The dialer places the call; the agent never dials. For each attempt the
//! dialer starts one agent in the attempt's LiveKit room. The campaign's
//! `dial_order` decides when: `customer_first` (the default) starts it when
//! the customer answers; `agent_first` starts it, waits until it is in the
//! room, and only then dials. Two runtimes:
//!
//! - `livekit`: a LiveKit Agents worker registered with an `agent_name`. The
//!   dialer creates an explicit dispatch with the call facts as job metadata.
//!   The agent joins as `agent-AJ_...` with kind AGENT.
//! - `pipecat`: a pipecat bot server. The dialer mints a room token for the
//!   identity `agent-attempt-{id}` and POSTs a start request to `start_url`:
//!
//!   ```json
//!   {"url": "<LiveKit URL>", "room_name": "<room>", "token": "<JWT>",
//!    "identity": "agent-attempt-42", "call": { ...call facts... }}
//!   ```
//!
//!   A 2xx answer means the bot accepted the call. The bot joins with
//!   pipecat's `LiveKitTransport(url, token, room_name)`.
//!
//! Both get the same call facts (`call_metadata`), so one agent design serves
//! both runtimes. When the dialer serves the session stream, the facts carry
//! `session: {url, token}`: the agent opens the stream with them and reports
//! the outcome of the call (`proto/agentdialer/session/v1/session.proto`). Adding a runtime is one `AgentSpec` variant and one match arm
//! in `Agents::start`.

use std::sync::Arc;
use std::time::Duration;

use ad_core::Clock;
use ad_livekit::{Api, Signer, VideoGrants};
use livekit_protocol::CreateAgentDispatchRequest;
use serde_json::json;

mod spec;
pub use spec::{AgentSpec, DialOrder, SpecError, DEFAULT_JOIN_TIMEOUT_S, DEFAULT_OUTCOME_WAIT_S};

/// What the agent learns about the call.
#[derive(Debug, Clone, Copy)]
pub struct CallFacts<'a> {
    pub attempt_id: i64,
    pub campaign_id: i64,
    pub lead_id: i64,
    pub room: &'a str,
    /// The identity of the callee's SIP participant in the room.
    pub callee_identity: &'a str,
    pub phone_number: &'a str,
    /// `leads.context` (the CSV extra columns).
    pub lead_context: &'a serde_json::Value,
    pub session: Option<SessionFacts<'a>>,
}

/// Where the agent reports the outcome of the call.
#[derive(Debug, Clone, Copy)]
pub struct SessionFacts<'a> {
    /// The gRPC URL of the session stream.
    pub url: &'a str,
    /// The secret of this attempt's session.
    pub token: &'a str,
}

/// The call facts as JSON: the LiveKit job metadata, and `call` in the pipecat
/// start request. Pure.
pub fn call_metadata(spec: &AgentSpec, call: &CallFacts<'_>) -> serde_json::Value {
    let mut facts = json!({
        "attempt_id": call.attempt_id,
        "campaign_id": call.campaign_id,
        "room": call.room,
        "callee_identity": call.callee_identity,
        "lead": {
            "id": call.lead_id,
            "phone_number": call.phone_number,
            "context": call.lead_context,
        },
        "metadata": spec.metadata(),
    });
    if let Some(s) = call.session {
        facts["session"] = json!({"url": s.url, "token": s.token});
    }
    facts
}

/// The room identity of a pipecat bot.
pub fn pipecat_identity(attempt_id: i64) -> String {
    format!("agent-attempt-{attempt_id}")
}

/// Where the agents reach LiveKit, and the key that signs their tokens.
#[derive(Debug, Clone)]
pub struct LiveKitAccess {
    pub url: String,
    pub api_key: String,
    pub api_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    #[error("the agent runtime refused the start: {0}")]
    Refused(String),
    #[error("the agent runtime is unreachable: {0}")]
    Unreachable(String),
}

/// The life of a pipecat bot's room token.
const BOT_TOKEN_TTL: Duration = Duration::from_secs(3600);

/// The runtimes. One value serves every campaign.
pub struct Agents {
    dispatch: Api,
    http: reqwest::Client,
    livekit: LiveKitAccess,
}

impl Agents {
    /// The tokens take their time from `clock` (OPS-9).
    pub fn new(livekit: LiveKitAccess, clock: Arc<dyn Clock>) -> Self {
        Self {
            dispatch: Api::new(
                &livekit.url,
                Signer::new(&livekit.api_key, &livekit.api_secret, clock),
            ),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("an HTTP client with a timeout builds"),
            livekit,
        }
    }

    /// Start the agent of `spec` for one call. Returns a detail for the
    /// attempt log (the dispatch id, or the bot identity).
    pub async fn start(
        &self,
        spec: &AgentSpec,
        call: &CallFacts<'_>,
    ) -> Result<String, AgentError> {
        let metadata = call_metadata(spec, call);
        match spec {
            AgentSpec::Livekit { agent_name, .. } => {
                let dispatch = self
                    .dispatch
                    .create_dispatch(CreateAgentDispatchRequest {
                        agent_name: agent_name.clone(),
                        room: call.room.to_string(),
                        metadata: metadata.to_string(),
                        ..Default::default()
                    })
                    .await
                    .map_err(|e| AgentError::Unreachable(e.to_string()))?;
                Ok(format!("livekit dispatch {}", dispatch.id))
            }
            AgentSpec::Pipecat { start_url, .. } => {
                let identity = pipecat_identity(call.attempt_id);
                let token = self.bot_token(call.room, &identity)?;
                let body = json!({
                    "url": self.livekit.url,
                    "room_name": call.room,
                    "token": token,
                    "identity": identity,
                    "call": metadata,
                });
                let resp = self
                    .http
                    .post(start_url)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| AgentError::Unreachable(e.to_string()))?;
                if resp.status().is_success() {
                    Ok(format!("pipecat bot {identity}"))
                } else {
                    Err(AgentError::Refused(format!(
                        "{start_url} answered {}",
                        resp.status()
                    )))
                }
            }
        }
    }

    /// A token that lets the bot join one room as an agent.
    fn bot_token(&self, room: &str, identity: &str) -> Result<String, AgentError> {
        self.dispatch
            .signer()
            .join(
                room,
                identity,
                Some("agent"),
                VideoGrants {
                    agent: true,
                    can_publish: Some(true),
                    can_subscribe: Some(true),
                    ..Default::default()
                },
                BOT_TOKEN_TTL,
            )
            .map_err(|e| AgentError::Refused(format!("token: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn livekit_spec() -> serde_json::Value {
        json!({"runtime": "livekit", "agent_name": "dialer-agent", "metadata": {"script": "a"}})
    }

    #[test]
    fn specs_parse_with_defaults() {
        let s = AgentSpec::parse(&livekit_spec()).unwrap();
        assert_eq!(s.runtime(), "livekit");
        assert_eq!(s.join_timeout_s(), DEFAULT_JOIN_TIMEOUT_S);
        assert_eq!(s.outcome_wait_s(), DEFAULT_OUTCOME_WAIT_S);
        let p = AgentSpec::parse(&json!({
            "runtime": "pipecat", "start_url": "http://bots:7860/start", "join_timeout_s": 5
        }))
        .unwrap();
        assert_eq!(p.runtime(), "pipecat");
        assert_eq!(p.join_timeout_s(), 5);
        assert_eq!(s.dial_order(), DialOrder::CustomerFirst);
        let a = AgentSpec::parse(&json!({
            "runtime": "pipecat", "start_url": "http://bots:7860/start", "dial_order": "agent_first"
        }))
        .unwrap();
        assert_eq!(a.dial_order(), DialOrder::AgentFirst);
    }

    #[test]
    fn the_pool_does_not_depend_on_the_dial_order() {
        let lk = AgentSpec::parse(&livekit_spec()).unwrap();
        let lk_af = AgentSpec::parse(&json!({
            "runtime": "livekit", "agent_name": "dialer-agent", "dial_order": "agent_first"
        }))
        .unwrap();
        assert_eq!(lk.pool(), "livekit:dialer-agent");
        assert_eq!(lk_af.pool(), lk.pool());
        let pc = AgentSpec::parse(&json!({
            "runtime": "pipecat", "start_url": "http://bots:7860/start?bot=slow"
        }))
        .unwrap();
        assert_eq!(pc.pool(), "pipecat:http://bots:7860/start?bot=slow");
    }

    #[test]
    fn bad_specs_are_refused() {
        for bad in [
            json!({"runtime": "livekit", "agent_name": " "}),
            json!({"runtime": "pipecat", "start_url": "ftp://x"}),
            json!({"runtime": "pipecat", "start_url": "http://x", "join_timeout_s": 0}),
            json!({"runtime": "pipecat", "start_url": "http://x", "outcome_wait_s": 601}),
            json!({"runtime": "vapi", "url": "x"}),
            json!({"runtime": "livekit", "agent_name": "a", "typo": 1}),
            json!({"runtime": "livekit", "agent_name": "a", "dial_order": "agent_last"}),
            json!({"agent_name": "a"}),
        ] {
            assert!(AgentSpec::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn both_runtimes_get_the_same_call_facts() {
        let context = json!({"first_name": "Ada"});
        let call = CallFacts {
            attempt_id: 42,
            campaign_id: 7,
            lead_id: 9,
            room: "campaign-42",
            callee_identity: "sip-attempt-42",
            phone_number: "+15550100",
            lead_context: &context,
            session: Some(SessionFacts {
                url: "http://api:9090",
                token: "t",
            }),
        };
        let lk = call_metadata(&AgentSpec::parse(&livekit_spec()).unwrap(), &call);
        assert_eq!(
            lk["session"],
            json!({"url": "http://api:9090", "token": "t"})
        );
        let without = call_metadata(
            &AgentSpec::parse(&livekit_spec()).unwrap(),
            &CallFacts {
                session: None,
                ..call
            },
        );
        assert!(without.get("session").is_none());
        assert_eq!(lk["attempt_id"], 42);
        assert_eq!(lk["callee_identity"], "sip-attempt-42");
        assert_eq!(lk["lead"]["context"]["first_name"], "Ada");
        assert_eq!(lk["metadata"]["script"], "a");
        let pc = call_metadata(
            &AgentSpec::parse(&json!({"runtime": "pipecat", "start_url": "http://x/start"}))
                .unwrap(),
            &call,
        );
        let strip = |mut v: serde_json::Value| {
            v.as_object_mut().unwrap().remove("metadata");
            v
        };
        assert_eq!(strip(lk), strip(pc));
    }
}
