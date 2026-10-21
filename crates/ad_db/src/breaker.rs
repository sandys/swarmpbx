//! The circuit-breaker window on Postgres (decision D-3). The window is the
//! `call_outcomes` table: writing an outcome row is the record. Evaluation reads
//! the window and replays it into the pure `ad_circuit_breaker` engine, which
//! stays the single source of the trip decision.

use ad_circuit_breaker::{CircuitBreaker, Config, Stats};
use ad_events::{BreakerStore, EventError};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

#[derive(Clone)]
pub struct PgBreakerStore {
    pool: PgPool,
}

impl PgBreakerStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn tx(e: sqlx::Error) -> EventError {
    EventError::Transport(e.to_string())
}

impl BreakerStore for PgBreakerStore {
    async fn evaluate(
        &self,
        campaign_id: i64,
        now: DateTime<Utc>,
        config: &Config,
    ) -> Result<(bool, Stats), EventError> {
        let rows = sqlx::query(
            "SELECT attempt_at, outcome = 'failed' AS failed FROM call_outcomes \
             WHERE campaign_id = $1 AND attempt_at >= $2 AND attempt_at <= $3",
        )
        .bind(campaign_id)
        .bind(now - config.window)
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(tx)?;
        let mut cb = CircuitBreaker::new();
        for row in rows {
            let at: DateTime<Utc> = row.try_get("attempt_at").map_err(tx)?;
            let failed: bool = row.try_get("failed").map_err(tx)?;
            cb.record(at, failed);
        }
        Ok(cb.is_open(now, config))
    }
}
