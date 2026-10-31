//! The DNC list on Postgres (GAPS.md COMP-7, COMP-1).
//!
//! An entry takes effect at once: in the same transaction, the leads with
//! that number that wait for a dial (`new` or `queued`, also a retry or a
//! callback) become `dnc`. The gate of the leader reads the entries of the
//! leads it claims, so a lead in another state cannot dial the number either.

use ad_compliance::dnc::DncSet;
use ad_contact::E164;
use ad_core::CampaignId;
use ad_disposition::DncScope;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Row, Transaction};

#[derive(Debug, thiserror::Error)]
pub enum DncError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("a campaign entry needs a campaign, and a system entry has none")]
    Scope,
}

fn err(e: impl std::fmt::Display) -> DncError {
    DncError::Storage(e.to_string())
}

/// A new entry.
#[derive(Debug, Clone)]
pub struct NewDnc {
    pub phone_number: E164,
    pub scope: DncScope,
    pub campaign_id: Option<CampaignId>,
    /// `agent` or `api`.
    pub source: &'static str,
    pub attempt_id: Option<i64>,
    pub evidence: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DncEntry {
    pub id: i64,
    pub phone_number: String,
    pub scope: String,
    pub campaign_id: Option<i64>,
    pub source: String,
    pub attempt_id: Option<i64>,
    pub evidence: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// What an add did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Added {
    /// The entry is new. An entry of the same number and scope can exist.
    pub created: bool,
    /// The waiting leads that became `dnc`.
    pub cancelled: u64,
}

/// Add an entry and stop the waiting leads of its number, in `tx`.
pub(crate) async fn add_in(
    tx: &mut Transaction<'_, Postgres>,
    new: &NewDnc,
) -> Result<Added, DncError> {
    if (new.scope == DncScope::Campaign) != new.campaign_id.is_some() {
        return Err(DncError::Scope);
    }
    let created = sqlx::query(
        "INSERT INTO dnc_numbers (phone_number, scope, campaign_id, source, attempt_id, evidence) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (phone_number, scope, COALESCE(campaign_id, 0)) DO NOTHING",
    )
    .bind(new.phone_number.as_str())
    .bind(new.scope.as_str())
    .bind(new.campaign_id.map(|c| c.0))
    .bind(new.source)
    .bind(new.attempt_id)
    .bind(&new.evidence)
    .execute(&mut **tx)
    .await
    .map_err(err)?
    .rows_affected()
        == 1;
    let cancelled = sqlx::query(
        "UPDATE leads SET status = 'dnc', next_eligible_at = NULL, retry_reason = NULL, \
             priority = 0, updated_at = now() \
         WHERE phone_number = $1 AND status IN ('new', 'queued') \
           AND ($2::BIGINT IS NULL OR campaign_id = $2)",
    )
    .bind(new.phone_number.as_str())
    .bind(new.campaign_id.map(|c| c.0))
    .execute(&mut **tx)
    .await
    .map_err(err)?
    .rows_affected();
    Ok(Added { created, cancelled })
}

#[derive(Clone)]
pub struct PgDncRepo {
    pool: PgPool,
}

impl PgDncRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn add(&self, new: &NewDnc) -> Result<Added, DncError> {
        let mut tx = self.pool.begin().await.map_err(err)?;
        let added = add_in(&mut tx, new).await?;
        tx.commit().await.map_err(err)?;
        Ok(added)
    }

    /// The entries of the system list and of one campaign, newest first.
    /// Without a campaign, every entry.
    pub async fn list(
        &self,
        campaign_id: Option<CampaignId>,
        limit: i64,
    ) -> Result<Vec<DncEntry>, DncError> {
        let rows = sqlx::query(
            "SELECT id, phone_number, scope, campaign_id, source, attempt_id, evidence, created_at \
             FROM dnc_numbers \
             WHERE $1::BIGINT IS NULL OR campaign_id IS NULL OR campaign_id = $1 \
             ORDER BY id DESC LIMIT $2",
        )
        .bind(campaign_id.map(|c| c.0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        rows.iter()
            .map(|r| {
                Ok(DncEntry {
                    id: r.try_get("id").map_err(err)?,
                    phone_number: r.try_get("phone_number").map_err(err)?,
                    scope: r.try_get("scope").map_err(err)?,
                    campaign_id: r.try_get("campaign_id").map_err(err)?,
                    source: r.try_get("source").map_err(err)?,
                    attempt_id: r.try_get("attempt_id").map_err(err)?,
                    evidence: r.try_get("evidence").map_err(err)?,
                    created_at: r.try_get("created_at").map_err(err)?,
                })
            })
            .collect()
    }

    /// Remove an entry. The leads that it stopped stay `dnc`.
    pub async fn remove(&self, id: i64) -> Result<bool, DncError> {
        let n = sqlx::query("DELETE FROM dnc_numbers WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(err)?
            .rows_affected();
        Ok(n == 1)
    }

    /// The entries that apply to `numbers` in one campaign, as the set that
    /// the compliance gate checks.
    pub async fn set_for(
        &self,
        campaign_id: CampaignId,
        numbers: &[E164],
    ) -> Result<DncSet, DncError> {
        let keys: Vec<&str> = numbers.iter().map(E164::as_str).collect();
        let rows = sqlx::query(
            "SELECT phone_number, scope FROM dnc_numbers \
             WHERE phone_number = ANY($1) AND (campaign_id IS NULL OR campaign_id = $2)",
        )
        .bind(&keys)
        .bind(campaign_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        let mut set = DncSet::default();
        for r in &rows {
            let number: String = r.try_get("phone_number").map_err(err)?;
            let scope: String = r.try_get("scope").map_err(err)?;
            let Some(key) = ad_contact::phone_key(&number) else {
                continue;
            };
            match scope.as_str() {
                "campaign" => set.campaign.insert(key),
                _ => set.system.insert(key),
            };
        }
        Ok(set)
    }
}
