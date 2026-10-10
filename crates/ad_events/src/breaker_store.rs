//! Persistent circuit-breaker state (PLAN.md §0.10.8a). The pure
//! `ad_circuit_breaker::CircuitBreaker` holds a rolling window in memory; that
//! window must survive across events and across orchestrator restarts, or the
//! breaker never trips (it was reset empty each tick — the bug this fixes).
//!
//! A `BreakerStore` persists per-campaign outcomes and evaluates the breaker by
//! replaying the recent window into the pure logic. The pure engine stays the
//! single source of the trip decision; this only persists its input.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use ad_circuit_breaker::{CircuitBreaker, Config, Stats};
use chrono::{DateTime, Utc};

use crate::EventError;

/// Persist call outcomes per campaign and evaluate the breaker over the window.
pub trait BreakerStore: Send + Sync {
    /// Record one terminal outcome for a campaign.
    fn record(
        &self,
        campaign_id: i64,
        at: DateTime<Utc>,
        failed: bool,
    ) -> impl Future<Output = Result<(), EventError>> + Send;

    /// Evaluate the breaker now. Returns (open, stats) from the pure engine.
    fn evaluate(
        &self,
        campaign_id: i64,
        now: DateTime<Utc>,
        config: &Config,
    ) -> impl Future<Output = Result<(bool, Stats), EventError>> + Send;
}

/// In-memory store: one pure breaker per campaign. For tests and single-process
/// use.
#[derive(Clone, Default)]
pub struct InMemoryBreakerStore {
    per_campaign: Arc<Mutex<HashMap<i64, CircuitBreaker>>>,
}

impl InMemoryBreakerStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BreakerStore for InMemoryBreakerStore {
    async fn record(
        &self,
        campaign_id: i64,
        at: DateTime<Utc>,
        failed: bool,
    ) -> Result<(), EventError> {
        let mut g = self.per_campaign.lock().unwrap();
        g.entry(campaign_id).or_default().record(at, failed);
        Ok(())
    }

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

#[cfg(feature = "redis")]
mod redis_impl {
    use super::*;
    use redis::AsyncCommands;

    fn tx(e: redis::RedisError) -> EventError {
        EventError::Transport(e.to_string())
    }

    fn key(campaign_id: i64) -> String {
        format!("ad:breaker:{campaign_id}")
    }

    /// Redis-backed store: outcomes in a per-campaign sorted set, score =
    /// epoch-ms. Each member is unique (`{epoch_ms}:{seq}:{f|s}`) so repeated
    /// outcomes at the same instant are not deduplicated by the set. Evaluation
    /// reads the window and replays it into the pure breaker.
    #[derive(Clone)]
    pub struct RedisBreakerStore {
        conn: redis::aio::MultiplexedConnection,
        seq: Arc<std::sync::atomic::AtomicU64>,
    }

    impl RedisBreakerStore {
        pub async fn connect(url: &str) -> Result<Self, EventError> {
            let client = redis::Client::open(url).map_err(tx)?;
            let conn = client
                .get_multiplexed_async_connection()
                .await
                .map_err(tx)?;
            Ok(Self {
                conn,
                seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            })
        }
    }

    impl BreakerStore for RedisBreakerStore {
        async fn record(
            &self,
            campaign_id: i64,
            at: DateTime<Utc>,
            failed: bool,
        ) -> Result<(), EventError> {
            let mut conn = self.conn.clone();
            let epoch_ms = at.timestamp_millis();
            let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let flag = if failed { 'f' } else { 's' };
            let member = format!("{epoch_ms}:{seq}:{flag}");
            let _: i64 = conn
                .zadd(key(campaign_id), member, epoch_ms)
                .await
                .map_err(tx)?;
            Ok(())
        }

        async fn evaluate(
            &self,
            campaign_id: i64,
            now: DateTime<Utc>,
            config: &Config,
        ) -> Result<(bool, Stats), EventError> {
            let mut conn = self.conn.clone();
            let k = key(campaign_id);
            let window_start = (now - config.window).timestamp_millis();
            let now_ms = now.timestamp_millis();

            // Trim outcomes older than the window so the set does not grow
            // unbounded, then read the live window.
            let _: i64 = conn
                .zrembyscore(&k, "-inf", format!("({window_start}"))
                .await
                .map_err(tx)?;
            let members: Vec<String> = conn
                .zrangebyscore(&k, window_start, now_ms)
                .await
                .map_err(tx)?;

            // Replay into the pure breaker. The `at` within the window does not
            // change the window-count decision, so we stamp each at `now`; the
            // failed flag is parsed from the member.
            let mut cb = CircuitBreaker::new();
            for m in members {
                let failed = m.ends_with(":f");
                cb.record(now, failed);
            }
            Ok(cb.is_open(now, config))
        }
    }
}

#[cfg(feature = "redis")]
pub use redis_impl::RedisBreakerStore;

#[cfg(test)]
mod breaker_store_tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_memory_store_trips_after_persisted_failures() {
        let store = InMemoryBreakerStore::new();
        let cfg = Config::default(); // 0.5 / 120s / min 5

        // Below min_calls: closed.
        for i in 0..4 {
            store.record(1, t(i), true).await.unwrap();
        }
        let (open, _) = store.evaluate(1, t(4), &cfg).await.unwrap();
        assert!(!open, "4 failures < min_calls 5");

        // Cross min_calls with a majority failed: trips. This is the key
        // property — the state PERSISTED across the separate record calls.
        store.record(1, t(5), true).await.unwrap();
        store.record(1, t(6), true).await.unwrap();
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
            store.record(1, t(i), true).await.unwrap(); // campaign 1 all fail
        }
        for i in 0..6 {
            store.record(2, t(i), false).await.unwrap(); // campaign 2 all ok
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
