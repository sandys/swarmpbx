//! Persistent circuit-breaker state (PLAN.md §0.10.8a). The pure
//! `ad_circuit_breaker::CircuitBreaker` holds a rolling window in memory; that
//! window must survive across events and across orchestrator restarts, or the
//! breaker never trips (it was reset empty each tick).
//!
//! A `BreakerStore` evaluates the breaker by replaying the recent window of
//! outcomes into the pure logic. The pure engine stays the single source of the
//! trip decision. In production the window is the ended call attempts
//! (`ad_db::PgBreakerStore`): the attempt row is the record, so the
//! trait only evaluates.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use ad_circuit_breaker::{CircuitBreaker, Config, Stats};
use chrono::{DateTime, Utc};

use crate::EventError;

/// Evaluate the breaker over a campaign's recent outcomes.
pub trait BreakerStore: Send + Sync {
    /// Evaluate the breaker now. Returns (open, stats) from the pure engine.
    fn evaluate(
        &self,
        campaign_id: i64,
        now: DateTime<Utc>,
        config: &Config,
    ) -> impl Future<Output = Result<(bool, Stats), EventError>> + Send;
}

/// In-memory store: one pure breaker per campaign. For tests.
#[derive(Clone, Default)]
pub struct InMemoryBreakerStore {
    per_campaign: Arc<Mutex<HashMap<i64, CircuitBreaker>>>,
}

impl InMemoryBreakerStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one terminal outcome for a campaign.
    pub fn record(&self, campaign_id: i64, at: DateTime<Utc>, failed: bool) {
        let mut g = self.per_campaign.lock().unwrap();
        g.entry(campaign_id).or_default().record(at, failed);
    }
}

impl BreakerStore for InMemoryBreakerStore {
    async fn evaluate(
        &self,
        campaign_id: i64,
        now: DateTime<Utc>,
        config: &Config,
    ) -> Result<(bool, Stats), EventError> {
        let g = self.per_campaign.lock().unwrap();
        match g.get(&campaign_id) {
            Some(cb) => Ok(cb.is_open(now, config)),
            None => Ok(CircuitBreaker::new().is_open(now, config)),
        }
    }
}

#[cfg(test)]
mod breaker_store_tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_memory_store_trips_after_recorded_failures() {
        let store = InMemoryBreakerStore::new();
        let cfg = Config::default(); // 0.5 / 120s / min 5

        // Below min_calls: closed.
        for i in 0..4 {
            store.record(1, t(i), true);
        }
        let (open, _) = store.evaluate(1, t(4), &cfg).await.unwrap();
        assert!(!open, "4 failures < min_calls 5");

        // Cross min_calls with a majority failed: trips. The window persisted
        // across the separate record calls.
        store.record(1, t(5), true);
        store.record(1, t(6), true);
        let (open, stats) = store.evaluate(1, t(6), &cfg).await.unwrap();
        assert!(open, "6 calls, all failed > 50% and >= 5");
        assert_eq!(stats.total, 6);
        assert_eq!(stats.failures, 6);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_memory_store_isolates_campaigns() {
        let store = InMemoryBreakerStore::new();
        let cfg = Config::default();
        for i in 0..6 {
            store.record(1, t(i), true); // campaign 1 all fail
        }
        for i in 0..6 {
            store.record(2, t(i), false); // campaign 2 all ok
        }
        assert!(
            store.evaluate(1, t(6), &cfg).await.unwrap().0,
            "campaign 1 open"
        );
        assert!(
            !store.evaluate(2, t(6), &cfg).await.unwrap().0,
            "campaign 2 closed"
        );
    }
}
