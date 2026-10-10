//! `ad_events` — the orchestrator event layer (PLAN.md §0.10.8a). Redis carries
//! the control signals and the time-ordered schedule; Postgres stays the
//! durable truth. This crate is the typed contract plus the transport seam.
//!
//! Shape follows dograh's `campaign_event_protocol.py` / `campaign_orchestrator.py`
//! (a single `CAMPAIGN_EVENTS` channel, a tagged event set). We model the event
//! set as one tagged enum for exhaustive matching, and keep the transport behind
//! traits so tests use in-memory fakes and production uses Redis.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The single pub/sub channel, matching dograh's `RedisChannel.CAMPAIGN_EVENTS`.
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

/// The campaign event set. `type` is the serde tag, so the JSON on the wire
/// matches dograh's `{"type": "...", "campaign_id": N, ...}` shape.
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
        /// `true` when the call connected (answered). Drives the circuit breaker.
        answered: bool,
    },
    /// A terminal call needs a retry; the orchestrator schedules a due-timer.
    RetryNeeded {
        campaign_id: i64,
        lead_id: i64,
        reason: RetryReason,
    },
    /// A retry was placed on the due-timer schedule.
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

/// A time-ordered due-timer schedule (retries, pacing wakeups). Backed by a
/// Redis sorted set in production; an in-memory map in tests.
pub trait DueTimers: Send + Sync {
    /// Schedule `lead_id` to become due at `due_epoch_ms` for a campaign.
    fn schedule(
        &self,
        campaign_id: i64,
        lead_id: i64,
        due_epoch_ms: i64,
    ) -> impl Future<Output = Result<(), EventError>> + Send;

    /// Pop all leads whose due time is `<= now_epoch_ms` for a campaign.
    fn pop_due(
        &self,
        campaign_id: i64,
        now_epoch_ms: i64,
    ) -> impl Future<Output = Result<Vec<i64>, EventError>> + Send;
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

/// In-memory due-timer schedule: a (campaign, due, lead) list.
#[derive(Clone, Default)]
pub struct InMemoryTimers {
    entries: Arc<Mutex<Vec<(i64, i64, i64)>>>, // (campaign_id, due_epoch_ms, lead_id)
}

impl InMemoryTimers {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DueTimers for InMemoryTimers {
    async fn schedule(
        &self,
        campaign_id: i64,
        lead_id: i64,
        due_epoch_ms: i64,
    ) -> Result<(), EventError> {
        self.entries
            .lock()
            .unwrap()
            .push((campaign_id, due_epoch_ms, lead_id));
        Ok(())
    }

    async fn pop_due(&self, campaign_id: i64, now_epoch_ms: i64) -> Result<Vec<i64>, EventError> {
        let mut g = self.entries.lock().unwrap();
        let mut due = Vec::new();
        g.retain(|(cid, when, lead)| {
            if *cid == campaign_id && *when <= now_epoch_ms {
                due.push(*lead);
                false
            } else {
                true
            }
        });
        Ok(due)
    }
}

#[cfg(feature = "redis")]
mod redis_transport;
#[cfg(feature = "redis")]
pub use redis_transport::{RedisConsumer, RedisPublisher, RedisTimers};

mod breaker_store;
#[cfg(feature = "redis")]
pub use breaker_store::RedisBreakerStore;
pub use breaker_store::{BreakerStore, InMemoryBreakerStore};

#[cfg(test)]
mod tests;
