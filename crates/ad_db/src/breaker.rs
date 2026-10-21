//! The circuit-breaker window on Postgres (decision D-3). The window is the
//! ended call attempts that placed a call. Evaluation reads the window and
//! replays it into the pure `ad_circuit_breaker` engine, which stays the single
//! source of the trip decision. A resume sets `campaigns.breaker_reset_at`, and
//! the window starts after it.

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
            "SELECT a.ended_at, a.failure FROM call_attempts a \
             JOIN campaigns c ON c.id = a.campaign_id \
             WHERE a.campaign_id = $1 AND a.dialed_at IS NOT NULL \
               AND a.ended_at >= $2 AND a.ended_at <= $3 \
               AND (c.breaker_reset_at IS NULL OR a.ended_at > c.breaker_reset_at)",
        )
        .bind(campaign_id)
        .bind(now - config.window)
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(tx)?;
        let mut cb = CircuitBreaker::new();
        for row in rows {
            let at: DateTime<Utc> = row.try_get("ended_at").map_err(tx)?;
            let failed: bool = row.try_get("failure").map_err(tx)?;
            cb.record(at, failed);
        }
        Ok(cb.is_open(now, config))
    }
}
