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
        }
    }
}

/// The campaign event set. `type` is the serde tag, so the JSON matches
/// dograh's `{"type": "...", "campaign_id": N, ...}` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CampaignEvent {
    /// A batch finished. The orchestrator schedules the next batch.
    BatchCompleted {
        campaign_id: i64,
        processed_count: u32,
        failed_count: u32,
        batch_size: u32,
    },
    /// A batch failed as a whole (not individual call failures).
    BatchFailed {
        campaign_id: i64,
        error: String,
        processed_count: u32,
    },
    /// One call reached a terminal state and its outcome is persisted.
    CallCompleted {
        campaign_id: i64,
        lead_id: i64,
        /// `true` when the call connected (answered).
        answered: bool,
    },
    /// A terminal call needs a retry. Informational: the decision is already
    /// persisted on the lead (`next_eligible_at`) when this is published.
    RetryNeeded {
        campaign_id: i64,
        lead_id: i64,
        reason: RetryReason,
    },
    /// A retry is scheduled. Informational, like `RetryNeeded`.
    RetryScheduled {
        campaign_id: i64,
        lead_id: i64,
        due_epoch_ms: i64,
    },
    /// The failure rate crossed the threshold; dialing pauses for the campaign.
    CircuitBreakerTripped { campaign_id: i64, failure_rate: f64 },
    /// No more work; the campaign is done.
    CampaignCompleted { campaign_id: i64 },
}

impl CampaignEvent {
    pub fn campaign_id(&self) -> i64 {
        match self {
            CampaignEvent::BatchCompleted { campaign_id, .. }
            | CampaignEvent::BatchFailed { campaign_id, .. }
            | CampaignEvent::CallCompleted { campaign_id, .. }
            | CampaignEvent::RetryNeeded { campaign_id, .. }
            | CampaignEvent::RetryScheduled { campaign_id, .. }
            | CampaignEvent::CircuitBreakerTripped { campaign_id, .. }
            | CampaignEvent::CampaignCompleted { campaign_id } => *campaign_id,
        }
    }

    /// The event type: the serde tag, stored in the event table's `kind` column.
    pub fn kind(&self) -> &'static str {
        match self {
            CampaignEvent::BatchCompleted { .. } => "batch_completed",
            CampaignEvent::BatchFailed { .. } => "batch_failed",
            CampaignEvent::CallCompleted { .. } => "call_completed",
            CampaignEvent::RetryNeeded { .. } => "retry_needed",
            CampaignEvent::RetryScheduled { .. } => "retry_scheduled",
            CampaignEvent::CircuitBreakerTripped { .. } => "circuit_breaker_tripped",
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
