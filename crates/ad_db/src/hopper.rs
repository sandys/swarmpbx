//! Postgres hopper repository. The claim uses `SELECT ... FOR UPDATE SKIP
//! LOCKED` so N dispatch workers pull disjoint batches with no double-claim and
//! automatic release on crash (the uncommitted lock is dropped). PLAN.md §0.8.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

use ad_core::{CampaignId, LeadId};

#[derive(Debug, thiserror::Error)]
pub enum HopperError {
    #[error("storage error: {0}")]
    Storage(String),
}

fn err(e: sqlx::Error) -> HopperError {
    HopperError::Storage(e.to_string())
}

/// A claimed hopper item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedItem {
    pub hopper_id: i64,
    pub lead_id: LeadId,
    pub claimed_by: String,
    pub claimed_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct PgHopperRepo {
    pool: PgPool,
}

impl PgHopperRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert leads into the hopper for a campaign. Idempotent on lead_id
    /// (ON CONFLICT DO NOTHING), so re-filling does not duplicate.
    pub async fn insert_leads(
        &self,
        campaign_id: CampaignId,
        lead_ids: &[LeadId],
        priority: i32,
    ) -> Result<u64, HopperError> {
        let mut inserted = 0u64;
        let mut tx = self.pool.begin().await.map_err(err)?;
        for lead_id in lead_ids {
            let res = sqlx::query(
                "INSERT INTO hopper (campaign_id, lead_id, priority) VALUES ($1,$2,$3) \
                 ON CONFLICT (lead_id) DO NOTHING",
            )
            .bind(campaign_id.0)
            .bind(lead_id.0)
            .bind(priority)
            .execute(&mut *tx)
            .await
            .map_err(err)?;
            inserted += res.rows_affected();
        }
        tx.commit().await.map_err(err)?;
        Ok(inserted)
    }

    /// Claim up to `limit` available hopper items for `worker`, atomically.
    /// Concurrent callers never receive the same item (SKIP LOCKED).
    pub async fn claim_batch(
        &self,
        campaign_id: CampaignId,
        worker: &str,
        limit: i64,
    ) -> Result<Vec<ClaimedItem>, HopperError> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        // Lock the next available rows, skipping any another txn holds.
        let rows = sqlx::query(
            "SELECT id, lead_id FROM hopper \
             WHERE campaign_id = $1 AND claimed_by IS NULL \
             ORDER BY priority DESC, id ASC \
             LIMIT $2 FOR UPDATE SKIP LOCKED",
        )
        .bind(campaign_id.0)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(err)?;

        let now = Utc::now();
        let mut claimed = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.try_get("id").map_err(err)?;
            let lead_id: i64 = row.try_get("lead_id").map_err(err)?;
            sqlx::query("UPDATE hopper SET claimed_by = $1, claimed_at = $2 WHERE id = $3")
                .bind(worker)
                .bind(now)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(err)?;
            claimed.push(ClaimedItem {
                hopper_id: id,
                lead_id: LeadId(lead_id),
                claimed_by: worker.to_string(),
                claimed_at: now,
            });
        }
        tx.commit().await.map_err(err)?;
        Ok(claimed)
    }

    /// Count available (unclaimed) items for a campaign.
    pub async fn available_count(&self, campaign_id: CampaignId) -> Result<i64, HopperError> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS n FROM hopper WHERE campaign_id = $1 AND claimed_by IS NULL",
        )
        .bind(campaign_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(err)?;
        row.try_get("n").map_err(err)
    }
}
