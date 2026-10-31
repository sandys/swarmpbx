//! `ad_events` — the orchestrator event contract (PLAN.md §0.10.8a). This crate
//! is the typed event set and the transport seam only. The Postgres transport
//! (an event table plus LISTEN/NOTIFY) lives in `ad_db::events`; tests use the
//! in-memory fakes here. Postgres is the durable truth; an event is a wake-up
//! and an audit record, never the only copy of state.
//!
//! Shape follows dograh's `campaign_event_protocol.py` / `campaign_orchestrator.py`
//! (a single `CAMPAIGN_EVENTS` channel, a tagged event set). We model the event
//! set as one tagged enum for exhaustive matching.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The single notification channel, matching dograh's `RedisChannel.CAMPAIGN_EVENTS`.
pub const CAMPAIGN_EVENTS_CHANNEL: &str = "campaign_events";

/// Why a terminal call must retry. Mirrors dograh's `RetryReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryReason {
    Busy,
    NoAnswer,
    Voicemail,
    Failed,
    Error,
    /// No agent was in the room in time (customer first). The retry dials
    /// agent first.
    Abandoned,
}

impl RetryReason {
    /// Stable storage string (the serde name).
    pub fn as_str(&self) -> &'static str {
        match self {
            RetryReason::Busy => "busy",
            RetryReason::NoAnswer => "no_answer",
            RetryReason::Voicemail => "voicemail",
            RetryReason::Failed => "failed",
            RetryReason::Error => "error",
            RetryReason::Abandoned => "abandoned",
        }
    }
}

/// The campaign event set. `type` is the serde tag, so the JSON matches
/// dograh's `{"type": "...", "campaign_id": N, ...}` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CampaignEvent {
    /// The campaign entered the running state (start or resume).
    CampaignRunning { campaign_id: i64 },
    /// A call attempt ended and its outcome is saved. The orchestrator updates
    /// the lead and dials into the freed capacity.
    AttemptEnded { campaign_id: i64, attempt_id: i64 },
    /// The agent of an attempt is in the room, so the attempt left the pacing
    /// window of its agent pool, and the campaign can dial again.
    AgentJoined { campaign_id: i64, attempt_id: i64 },
    /// The lead is updated from an ended attempt that started a dial. The
    /// attempt counts in `called_count`.
    CallCompleted {
        campaign_id: i64,
        lead_id: i64,
        /// `true` when the callee answered.
        answered: bool,
    },
    /// A retry is scheduled. Informational: the due time is on the lead
    /// (`next_eligible_at`).
    RetryScheduled {
        campaign_id: i64,
        lead_id: i64,
        due_epoch_ms: i64,
    },
    /// The failure rate crossed the threshold, and the campaign is paused.
    CircuitBreakerTripped { campaign_id: i64, failure_rate: f64 },
    /// The agents of the campaign's pool did not start: `failed_in_row`
    /// starts in a row failed at a limit of 1 (GAPS.md DIAL-9 item 7). The
    /// campaign is paused.
    AgentPoolTripped {
        campaign_id: i64,
        pool: String,
        failed_in_row: u32,
        /// The last attempts of the run, oldest first.
        attempt_ids: Vec<i64>,
    },
    /// The agent of an attempt reported its outcome. A DNC request in it
    /// took effect in the same transaction.
    AgentOutcome {
        campaign_id: i64,
        attempt_id: i64,
        disposition: Option<String>,
        dnc: bool,
    },
    /// No work is left, and the campaign is completed.
    CampaignCompleted { campaign_id: i64 },
}

impl CampaignEvent {
    pub fn campaign_id(&self) -> i64 {
        match self {
            CampaignEvent::CampaignRunning { campaign_id }
            | CampaignEvent::AttemptEnded { campaign_id, .. }
            | CampaignEvent::AgentJoined { campaign_id, .. }
            | CampaignEvent::CallCompleted { campaign_id, .. }
            | CampaignEvent::RetryScheduled { campaign_id, .. }
            | CampaignEvent::CircuitBreakerTripped { campaign_id, .. }
            | CampaignEvent::AgentPoolTripped { campaign_id, .. }
            | CampaignEvent::AgentOutcome { campaign_id, .. }
            | CampaignEvent::CampaignCompleted { campaign_id } => *campaign_id,
        }
    }

    /// The event type: the serde tag, stored in the event table's `kind` column.
    pub fn kind(&self) -> &'static str {
        match self {
            CampaignEvent::CampaignRunning { .. } => "campaign_running",
            CampaignEvent::AttemptEnded { .. } => "attempt_ended",
            CampaignEvent::AgentJoined { .. } => "agent_joined",
            CampaignEvent::CallCompleted { .. } => "call_completed",
            CampaignEvent::RetryScheduled { .. } => "retry_scheduled",
            CampaignEvent::CircuitBreakerTripped { .. } => "circuit_breaker_tripped",
            CampaignEvent::AgentPoolTripped { .. } => "agent_pool_tripped",
            CampaignEvent::AgentOutcome { .. } => "agent_outcome",
            CampaignEvent::CampaignCompleted { .. } => "campaign_completed",
        }
    }

    pub fn to_json(&self) -> String {
        // The variants are plain data; serialization cannot fail.
        serde_json::to_string(self).expect("CampaignEvent serializes")
    }

    pub fn from_json(s: &str) -> Result<Self, EventError> {
        serde_json::from_str(s).map_err(|e| EventError::Decode(e.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventError {
    Decode(String),
    Transport(String),
}

impl std::fmt::Display for EventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventError::Decode(e) => write!(f, "event decode error: {e}"),
            EventError::Transport(e) => write!(f, "event transport error: {e}"),
        }
    }
}
impl std::error::Error for EventError {}

/// Publish a campaign event to the shared channel.
pub trait Publisher: Send + Sync {
    fn publish(&self, event: &CampaignEvent)
        -> impl Future<Output = Result<(), EventError>> + Send;
}

/// Receive the next campaign event (blocking until one arrives or the transport
/// closes). The orchestrator leader drives this.
pub trait Consumer: Send {
    fn next_event(
        &mut self,
    ) -> impl Future<Output = Result<Option<CampaignEvent>, EventError>> + Send;
}

// ---------------------------------------------------------------------------
// In-memory fakes for tests. A shared bus so a publisher and a consumer in the
// same test see the same events.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct InMemoryBus {
    queue: Arc<Mutex<VecDeque<CampaignEvent>>>,
}

impl InMemoryBus {
    pub fn new() -> Self {
        Self::default()
    }
    /// Count of events still unread (test introspection).
    pub fn pending(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

impl Publisher for InMemoryBus {
    async fn publish(&self, event: &CampaignEvent) -> Result<(), EventError> {
        self.queue.lock().unwrap().push_back(event.clone());
        Ok(())
    }
}

impl Consumer for InMemoryBus {
    async fn next_event(&mut self) -> Result<Option<CampaignEvent>, EventError> {
        Ok(self.queue.lock().unwrap().pop_front())
    }
}

mod breaker_store;
pub use breaker_store::{BreakerStore, InMemoryBreakerStore};

#[cfg(test)]
mod tests;
