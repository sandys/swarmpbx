//! Singleton leadership and concurrency capacity on Postgres (PLAN.md §0.8).
//!
//! `try_become_leader` uses a session-level `pg_try_advisory_lock`: only one
//! holder succeeds; the lock is held for the life of the returned guard's
//! connection and released when it drops. This elects the single orchestrator
//! without a custom lease.
//!
//! `in_call_count` reports how many of a campaign's hopper items are currently
//! INCALL (claimed), i.e. occupied concurrency — the signal the dispatcher uses
//! to bound progressive pacing.

use sqlx::{PgPool, Row};

use ad_core::CampaignId;

#[derive(Debug, thiserror::Error)]
pub enum LeaderError {
    #[error("storage error: {0}")]
    Storage(String),
}

fn err(e: sqlx::Error) -> LeaderError {
    LeaderError::Storage(e.to_string())
}

/// Holds a session-level advisory lock. Dropping it returns the dedicated
/// connection to the pool, which releases the session lock.
pub struct LeaderGuard {
    // Keeping the connection alive holds the session advisory lock. We never
    // read it again; the Drop of the connection releases the lock.
    _conn: sqlx::pool::PoolConnection<sqlx::Postgres>,
    key: i64,
}

impl LeaderGuard {
    pub fn key(&self) -> i64 {
        self.key
    }
}

/// Try to acquire the advisory lock for `key`. Returns `Some(guard)` if this
/// caller is now the leader, `None` if another holder has it.
pub async fn try_become_leader(
    pool: &PgPool,
    key: i64,
) -> Result<Option<LeaderGuard>, LeaderError> {
    // A dedicated connection: a session-level lock lives on its connection.
    let mut conn = pool.acquire().await.map_err(err)?;
    let row = sqlx::query("SELECT pg_try_advisory_lock($1) AS locked")
        .bind(key)
        .fetch_one(&mut *conn)
        .await
        .map_err(err)?;
    let locked: bool = row.try_get("locked").map_err(err)?;
    if locked {
        Ok(Some(LeaderGuard { _conn: conn, key }))
    } else {
        // Return the connection to the pool; we did not get the lock.
        Ok(None)
    }
}

/// Count a campaign's in-call (claimed) hopper items — occupied concurrency.
pub async fn in_call_count(pool: &PgPool, campaign: CampaignId) -> Result<i64, LeaderError> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM hopper WHERE campaign_id = $1 AND claimed_by IS NOT NULL",
    )
    .bind(campaign.0)
    .fetch_one(pool)
    .await
    .map_err(err)?;
    row.try_get("n").map_err(err)
}
