//! The agent of a campaign, as data (`campaigns.agent`). The `runtime` field
//! names the adapter, so a new runtime needs no migration.

use serde::{Deserialize, Serialize};

/// The default wait for the agent to join the room (agent first).
pub const DEFAULT_JOIN_TIMEOUT_S: u32 = 20;

fn default_join_timeout() -> u32 {
    DEFAULT_JOIN_TIMEOUT_S
}

/// The default wait for the agent's outcome after the call ends.
pub const DEFAULT_OUTCOME_WAIT_S: u32 = 30;

fn default_outcome_wait() -> u32 {
    DEFAULT_OUTCOME_WAIT_S
}

/// When the dialer starts the agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialOrder {
    /// Dial the customer, and start the agent when the customer answers. An
    /// agent that is not in the room within the abandonment rule's limit
    /// (two seconds) abandons the call.
    #[default]
    CustomerFirst,
    /// Start the agent, wait until it is in the room (`join_timeout_s`), and
    /// only then dial. No call is abandoned.
    AgentFirst,
}

impl DialOrder {
    pub fn name(&self) -> &'static str {
        match self {
            DialOrder::CustomerFirst => "customer_first",
            DialOrder::AgentFirst => "agent_first",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "runtime", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentSpec {
    /// A LiveKit Agents worker registered with `agent_name`. The dialer
    /// dispatches it into the room (`CreateDispatch`).
    Livekit {
        agent_name: String,
        /// Free-form data for the agent, in the job metadata.
        #[serde(default)]
        metadata: serde_json::Value,
        #[serde(default)]
        dial_order: DialOrder,
        #[serde(default = "default_join_timeout")]
        join_timeout_s: u32,
        /// After an answered call that the agent joined, the dialer waits
        /// this long for the agent's outcome before it settles the lead
        /// without one.
        #[serde(default = "default_outcome_wait")]
        outcome_wait_s: u32,
    },
    /// A pipecat bot server. The dialer POSTs a start request with a room
    /// token to `start_url`; the bot joins with pipecat's LiveKit transport.
    Pipecat {
        start_url: String,
        #[serde(default)]
        metadata: serde_json::Value,
        #[serde(default)]
        dial_order: DialOrder,
        #[serde(default = "default_join_timeout")]
        join_timeout_s: u32,
        /// After an answered call that the agent joined, the dialer waits
        /// this long for the agent's outcome before it settles the lead
        /// without one.
        #[serde(default = "default_outcome_wait")]
        outcome_wait_s: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid agent: {0}")]
pub struct SpecError(pub String);

impl AgentSpec {
    /// Read and check a stored or submitted agent.
    pub fn parse(value: &serde_json::Value) -> Result<Self, SpecError> {
        let spec: AgentSpec =
            serde_json::from_value(value.clone()).map_err(|e| SpecError(e.to_string()))?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        match self {
            AgentSpec::Livekit { agent_name, .. } if agent_name.trim().is_empty() => {
                Err(SpecError("agent_name must not be empty".into()))
            }
            AgentSpec::Pipecat { start_url, .. }
                if !(start_url.starts_with("http://") || start_url.starts_with("https://")) =>
            {
                Err(SpecError("start_url must be an http or https URL".into()))
            }
            _ if self.join_timeout_s() == 0 || self.join_timeout_s() > 300 => {
                Err(SpecError("join_timeout_s must be between 1 and 300".into()))
            }
            _ if self.outcome_wait_s() > 600 => {
                Err(SpecError("outcome_wait_s must be between 0 and 600".into()))
            }
            _ => Ok(()),
        }
    }

    pub fn runtime(&self) -> &'static str {
        match self {
            AgentSpec::Livekit { .. } => "livekit",
            AgentSpec::Pipecat { .. } => "pipecat",
        }
    }

    pub fn dial_order(&self) -> DialOrder {
        match self {
            AgentSpec::Livekit { dial_order, .. } | AgentSpec::Pipecat { dial_order, .. } => {
                *dial_order
            }
        }
    }

    /// The agent pool: the agents that share one capacity, and so one pacing
    /// window (GAPS.md DIAL-9).
    pub fn pool(&self) -> String {
        match self {
            AgentSpec::Livekit { agent_name, .. } => format!("livekit:{agent_name}"),
            AgentSpec::Pipecat { start_url, .. } => format!("pipecat:{start_url}"),
        }
    }

    pub fn join_timeout_s(&self) -> u32 {
        match self {
            AgentSpec::Livekit { join_timeout_s, .. }
            | AgentSpec::Pipecat { join_timeout_s, .. } => *join_timeout_s,
        }
    }

    pub fn outcome_wait_s(&self) -> u32 {
        match self {
            AgentSpec::Livekit { outcome_wait_s, .. }
            | AgentSpec::Pipecat { outcome_wait_s, .. } => *outcome_wait_s,
        }
    }

    pub fn metadata(&self) -> &serde_json::Value {
        match self {
            AgentSpec::Livekit { metadata, .. } | AgentSpec::Pipecat { metadata, .. } => metadata,
        }
    }
}
